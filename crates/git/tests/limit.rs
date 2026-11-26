//! What a repository can make `crates/git` allocate under a limit. Zlib
//! inflates about 1000 to 1, so each test plants a small object that
//! declares, and inflates to, far more than the limit, and counts what the
//! read allocates on its thread. A sparse file costs almost nothing on disk,
//! so others plant a file whose length is far more than the limit.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use git::{Error, Id, Kind, Object, Repository, Target};

mod plant;

use plant::{delta_header, entry_header, plant_pack, ref_delta, whole};

/// Counts the bytes each thread holds, and the most it has held.
struct Counting;

thread_local! {
    static HELD: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn count(change: isize) {
    let _ = HELD.try_with(|held| {
        held.set(held.get() + change);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(held.get())));
    });
}

// SAFETY: every call passes its arguments to `System` unchanged, and returns
// what `System` returned. Counting touches no memory that it hands out.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc`, which is
        // the contract of `System` too.
        let at = unsafe { System.alloc(layout) };
        if !at.is_null() {
            count(layout.size() as isize);
        }
        at
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc`, which is
        // the contract of `System` too.
        let at = unsafe { System.alloc_zeroed(layout) };
        if !at.is_null() {
            count(layout.size() as isize);
        }
        at
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        // SAFETY: the caller keeps the contract of `GlobalAlloc`, which is
        // the contract of `System` too.
        unsafe { System.dealloc(at, layout) };
        count(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller keeps the contract of `GlobalAlloc`, which is
        // the contract of `System` too.
        let moved = unsafe { System.realloc(at, layout, size) };
        if !moved.is_null() {
            count(size as isize - layout.size() as isize);
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// What `read` returns, and the most it held at once beyond what its
/// thread held before.
fn peak<T>(read: impl FnOnce() -> T) -> (T, usize) {
    let start = HELD.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let out = read();
    (out, (PEAK.with(Cell::get) - start) as usize)
}

const LIMIT: usize = 1 << 20;
const LARGE: usize = 64 << 20;

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("git-limit")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join(".git")).unwrap();
    dir
}

fn id(byte: u8) -> Id {
    Id([byte; 20])
}

/// The read must fail as corrupt, and print no contents if it doesn't.
fn assert_corrupt(result: Result<Option<Object>, Error>) {
    let read = result.map(|object| object.map(|object| object.data.len()));
    assert!(matches!(read, Err(Error::Corrupt(_))), "{read:?}");
}

fn assert_refused(result: Result<Option<Object>, Error>, held: usize) {
    assert_corrupt(result);
    assert!(held < LIMIT, "held {held} bytes");
}

#[test]
fn a_loose_object_larger_than_the_limit_is_refused_before_it_inflates() {
    let root = scratch("loose");
    let git = root.join(".git");
    let large = Repository::at(&git)
        .write(&Object {
            kind: Kind::Blob,
            data: vec![0; LARGE],
        })
        .unwrap();
    let (result, held) = peak(|| Repository::at(&git).with_limit(LIMIT).object(&large));
    assert_refused(result, held);
    // The limit is the size of the contents, without the header.
    let data = vec![7; LIMIT];
    let fits = Repository::at(&git)
        .write(&Object {
            kind: Kind::Blob,
            data: data.clone(),
        })
        .unwrap();
    let read = Repository::at(&git).with_limit(LIMIT).object(&fits);
    assert_eq!(read.unwrap().unwrap().data, data);
    let over = Repository::at(&git).with_limit(LIMIT - 1).object(&fits);
    assert_corrupt(over);
}

#[test]
fn a_packed_object_larger_than_the_limit_is_refused_before_it_inflates() {
    let root = scratch("packed");
    plant_pack(
        &root,
        &[
            (id(1), whole(3, &vec![0; LARGE])),
            (id(2), whole(3, &vec![7; LIMIT])),
        ],
    );
    let git = root.join(".git");
    let (result, held) = peak(|| Repository::at(&git).with_limit(LIMIT).object(&id(1)));
    assert_refused(result, held);
    let read = Repository::at(&git).with_limit(LIMIT).object(&id(2));
    assert_eq!(read.unwrap().unwrap().data, vec![7; LIMIT]);
    let over = Repository::at(&git).with_limit(LIMIT - 1).object(&id(2));
    assert_corrupt(over);
}

#[test]
fn a_packed_entry_longer_than_its_header_says_is_refused_before_it_grows() {
    let root = scratch("longer");
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0; LARGE], 6);
    plant_pack(
        &root,
        &[(id(1), [entry_header(3, LIMIT), compressed].concat())],
    );
    let git = root.join(".git");
    let (result, held) = peak(|| Repository::at(&git).with_limit(LIMIT).object(&id(1)));
    assert_corrupt(result);
    // The entry's own size, and buffers to inflate it, but never more.
    assert!(held < LIMIT * 3 / 2, "held {held} bytes");
}

#[test]
fn a_delta_whose_result_is_larger_than_the_limit_is_refused_before_it_applies() {
    let root = scratch("delta");
    // Each copy of 0x10000 bytes takes one byte of the delta.
    let copies = vec![0x80; LARGE / 0x10000];
    let delta = [delta_header(0x10000, LARGE), copies].concat();
    plant_pack(
        &root,
        &[
            (id(1), whole(3, &vec![0; 0x10000])),
            (id(2), ref_delta(&id(1), &delta)),
        ],
    );
    let git = root.join(".git");
    let (result, held) = peak(|| Repository::at(&git).with_limit(LIMIT).object(&id(2)));
    assert_refused(result, held);
    let read = Repository::at(&git).with_limit(LARGE).object(&id(2));
    assert_eq!(read.unwrap().unwrap().data.len(), LARGE);
}

#[test]
fn a_chain_of_deltas_is_applied_one_delta_at_a_time() {
    let root = scratch("chain");
    let size = LIMIT / 8;
    // Each delta inserts the whole result, 127 bytes at a time.
    let mut inserts = Vec::new();
    for piece in vec![9; size].chunks(127) {
        inserts.push(piece.len() as u8);
        inserts.extend(piece);
    }
    let delta = [delta_header(size, size), inserts].concat();
    let mut entries = vec![(id(0), whole(3, &vec![0; size]))];
    for n in 1..=64 {
        entries.push((id(n), ref_delta(&id(n - 1), &delta)));
    }
    plant_pack(&root, &entries);
    let git = root.join(".git");
    let (result, held) = peak(|| Repository::at(&git).with_limit(LIMIT).object(&id(64)));
    assert_eq!(result.unwrap().unwrap().data, vec![9; size]);
    // The base, one delta, and the result, where holding the whole chain
    // would be 64 deltas.
    assert!(held < LIMIT, "held {held} bytes");
}

/// The length of a sparse file, far more than the limit.
const SPARSE: u64 = 1 << 30;

/// Writes `start` to `path`, and makes the file `SPARSE` bytes long.
fn sparse(path: &Path, start: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, start).unwrap();
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_len(SPARSE).unwrap();
}

fn loose_path(git: &Path, id: &Id) -> PathBuf {
    let hex = id.to_string();
    git.join("objects").join(&hex[..2]).join(&hex[2..])
}

#[test]
fn a_sparse_loose_object_is_read_a_piece_at_a_time() {
    let root = scratch("sparse-loose");
    let git = root.join(".git");
    let repo = || Repository::at(&git).with_limit(LIMIT);
    // Git reads the object, and ignores what follows its zlib stream.
    let raw = miniz_oxide::deflate::compress_to_vec_zlib(b"blob 4\0four", 6);
    sparse(&loose_path(&git, &id(1)), &raw);
    let (result, held) = peak(|| repo().object(&id(1)));
    assert_eq!(result.unwrap().unwrap().data, b"four");
    assert!(held < LIMIT, "held {held} bytes");
    let (result, held) = peak(|| repo().kind(&id(1)));
    assert_eq!(result.unwrap(), Some(Kind::Blob));
    assert!(held < LIMIT, "held {held} bytes");
    // A file of nothing isn't zlib.
    sparse(&loose_path(&git, &id(2)), b"");
    let (result, held) = peak(|| repo().object(&id(2)));
    assert_refused(result, held);
    let (result, held) = peak(|| repo().kind(&id(2)));
    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert!(held < LIMIT, "held {held} bytes");
}

#[test]
fn a_sparse_pack_index_is_refused_before_it_is_read() {
    let root = scratch("sparse-index");
    plant_pack(&root, &[(id(1), whole(3, b"four"))]);
    let git = root.join(".git");
    let index = git.join("objects/pack/pack-planted.idx");
    let planted = fs::read(&index).unwrap();
    sparse(&index, &planted);
    let (result, held) = peak(|| Repository::at(&git).with_limit(LIMIT).object(&id(1)));
    assert_refused(result, held);
}

#[test]
fn sparse_refs_are_refused_before_they_are_read() {
    let root = scratch("sparse-refs");
    let git = root.join(".git");
    let repo = || Repository::at(&git).with_limit(LIMIT);
    let line = format!("{} refs/heads/packed\n", id(1));
    sparse(&git.join("packed-refs"), line.as_bytes());
    let (result, held) = peak(|| repo().find_ref("refs/heads/packed"));
    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert!(held < LIMIT, "held {held} bytes");
    let (result, held) = peak(|| repo().refs());
    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert!(held < LIMIT, "held {held} bytes");
    fs::remove_file(git.join("packed-refs")).unwrap();
    // Git would read this ref: it ends at the first NUL.
    let loose = git.join("refs/heads/loose");
    sparse(&loose, format!("{}\n", id(1)).as_bytes());
    let (result, held) = peak(|| repo().find_ref("refs/heads/loose"));
    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert!(held < LIMIT, "held {held} bytes");
    // A ref as long as the limit reads, and holds about that much.
    let file = fs::OpenOptions::new().write(true).open(&loose).unwrap();
    file.set_len(LIMIT as u64).unwrap();
    let (result, held) = peak(|| repo().find_ref("refs/heads/loose"));
    assert_eq!(result.unwrap(), Some(Target::Object(id(1))));
    assert!(held < LIMIT * 3 / 2, "held {held} bytes");
    // One byte longer is refused, and reads without a limit.
    file.set_len(LIMIT as u64 + 1).unwrap();
    let result = repo().find_ref("refs/heads/loose");
    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    let read = Repository::at(&git).find_ref("refs/heads/loose").unwrap();
    assert_eq!(read, Some(Target::Object(id(1))));
}

#[test]
fn pack_indexes_larger_than_the_limit_in_all_are_refused_before_one_is_read() {
    let root = scratch("indexes");
    plant_pack(&root, &[(id(1), whole(3, b"four"))]);
    let git = root.join(".git");
    let dir = git.join("objects/pack");
    let planted = dir.join("pack-planted");
    let indexes: Vec<PathBuf> = (0..20)
        .map(|n| {
            let pack = dir.join(format!("pack-{n:02}"));
            fs::copy(planted.with_extension("pack"), pack.with_extension("pack")).unwrap();
            fs::copy(planted.with_extension("idx"), pack.with_extension("idx")).unwrap();
            pack.with_extension("idx")
        })
        .collect();
    fs::remove_file(planted.with_extension("pack")).unwrap();
    fs::remove_file(planted.with_extension("idx")).unwrap();
    let each = fs::metadata(&indexes[0]).unwrap().len() as usize;
    // Each index is within the limit, and together they are as long as it.
    let all = each * indexes.len();
    let read = Repository::at(&git).with_limit(all).object(&id(1));
    assert_eq!(read.unwrap().unwrap().data, b"four");
    // A byte less, and the reader holds less than the limit.
    let (result, held) = peak(|| Repository::at(&git).with_limit(all - 1).object(&id(1)));
    assert_corrupt(result);
    assert!(held < all - 1, "held {held} bytes");
    // Sparse, each is half the limit long, and none is read.
    for index in &indexes {
        let file = fs::OpenOptions::new().write(true).open(index).unwrap();
        file.set_len(LIMIT as u64 / 2).unwrap();
    }
    let (result, held) = peak(|| Repository::at(&git).with_limit(LIMIT).object(&id(1)));
    assert_corrupt(result);
    assert!(held < LIMIT / 2, "held {held} bytes");
}
