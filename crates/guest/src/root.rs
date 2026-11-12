//! How the host tells the init which disks hold an agent's layers, and in
//! what order to stack them.
//!
//! Each layer is a read-only virtio-blk disk whose serial names it. The
//! kernel command line lists those names, bottom first, so the order comes
//! from the host and never from the order the kernel found the disks in.

/// The kernel command line parameter that lists the layers. A dotted name
/// reads as a module's parameter, so the kernel passes it to no one.
const PARAMETER: &str = "ap.layers=";

/// How much of a layer's digest names its disk: all a virtio-blk serial holds.
pub const NAME_LEN: usize = 20;

/// The name of the disk that serves the layer with this digest.
pub fn name(digest: &str) -> &str {
    digest.get(..NAME_LEN).unwrap_or(digest)
}

/// The command line parameter that lists these disk names, bottom first.
pub fn parameter(names: &[&str]) -> String {
    format!("{PARAMETER}{}", names.join(","))
}

/// The disk names the command line lists, bottom first. None when it lists
/// none, and the guest's root stays the initramfs.
pub fn order(command_line: &str) -> Vec<&str> {
    command_line
        .split_ascii_whitespace()
        .filter_map(|word| word.strip_prefix(PARAMETER))
        .last()
        .map(|list| list.split(',').filter(|name| !name.is_empty()).collect())
        .unwrap_or_default()
}

/// For each name in `order`, the device that serves it, out of the devices
/// found and their serials. Two layers with one digest may share a name, so
/// each device is used once. Returns the first name with no device.
pub fn devices<'a>(order: &[&'a str], found: &[(String, String)]) -> Result<Vec<String>, &'a str> {
    let mut unused: Vec<&(String, String)> = found.iter().collect();
    order
        .iter()
        .map(|name| {
            let at = unused
                .iter()
                .position(|(_, serial)| serial == name)
                .ok_or(*name)?;
            Ok(unused.remove(at).0.clone())
        })
        .collect()
}

/// overlayfs's options for these lower directories, given bottom first. Its
/// `lowerdir` lists the top first.
pub fn overlay_options(lower: &[String], upper: &str, work: &str) -> String {
    let lower: Vec<&str> = lower.iter().rev().map(String::as_str).collect();
    format!(
        "lowerdir={},upperdir={upper},workdir={work}",
        lower.join(":")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_order_survives_the_command_line() {
        let line = format!("console=ttyS0 {} reboot=t", parameter(&["b", "a", "c"]));
        assert_eq!(order(&line), ["b", "a", "c"]);
    }

    #[test]
    fn a_command_line_without_layers_lists_none() {
        assert!(order("console=ttyS0 reboot=t").is_empty());
        assert!(order("ap.layers=").is_empty());
        assert!(order("xap.layers=a").is_empty());
    }

    #[test]
    fn a_name_is_the_start_of_a_digest() {
        let digest = "0123456789abcdef0123456789abcdef";
        assert_eq!(name(digest), "0123456789abcdef0123");
        assert_eq!(name("short"), "short");
    }

    #[test]
    fn devices_follow_the_order_and_not_the_probe() {
        let found = [
            ("vda".to_owned(), "top".to_owned()),
            ("vdb".to_owned(), "bottom".to_owned()),
        ];
        assert_eq!(devices(&["bottom", "top"], &found).unwrap(), ["vdb", "vda"]);
    }

    #[test]
    fn layers_that_share_a_digest_each_get_a_device() {
        let found = [
            ("vda".to_owned(), "same".to_owned()),
            ("vdb".to_owned(), "same".to_owned()),
        ];
        assert_eq!(devices(&["same", "same"], &found).unwrap(), ["vda", "vdb"]);
        assert_eq!(devices(&["same", "same", "same"], &found), Err("same"));
    }

    #[test]
    fn a_layer_with_no_device_is_named() {
        let found = [("vda".to_owned(), "here".to_owned())];
        assert_eq!(devices(&["here", "gone"], &found), Err("gone"));
    }

    #[test]
    fn the_last_layer_is_on_top() {
        let lower = ["/layers/0".to_owned(), "/layers/1".to_owned()];
        assert_eq!(
            overlay_options(&lower, "/upper/upper", "/upper/work"),
            "lowerdir=/layers/1:/layers/0,upperdir=/upper/upper,workdir=/upper/work"
        );
    }
}
