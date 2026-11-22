//! A hidden secret's value, put in a request's head, by **Hidden** under
//! SECRET in SPEC.md. The proxy calls it on a TLS connection to a bound
//! host whose server name names that host, with each head that the guest
//! sends on it. The head has been read already, so its lines are whole.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// A hidden secret's placeholder, and the value that replaces it.
#[derive(Clone, Copy, Debug)]
pub struct Swap<'a> {
    pub placeholder: &'a [u8],
    pub value: &'a [u8],
}

/// Adds `head`, a request head on a connection to `name`, to `out`, with
/// each placeholder of `swaps` replaced by its value in the headers that
/// `headers` names, in any case. In an `Authorization: Basic` header, it
/// replaces the placeholder in the decoded credentials, which it encodes
/// again. A head whose `Host` names another host is added unchanged. A
/// value that holds a line break replaces no placeholder outside `Basic`
/// credentials, so no line break is ever put in a header. A head that would
/// grow past [`LONGEST`] is added unchanged too.
pub fn inject(head: &[u8], name: &str, headers: &[&str], swaps: &[Swap], out: &mut Vec<u8>) {
    if !hosts_name(head, name) {
        out.extend_from_slice(head);
        return;
    }
    let mut rewritten = Vec::with_capacity(head.len());
    rewrite(head, headers, swaps, &mut rewritten);
    match rewritten.len() > LONGEST {
        true => out.extend_from_slice(head),
        false => out.append(&mut rewritten),
    }
}

/// The longest head that [`inject`] makes. A guest's head with many
/// placeholders would otherwise grow without bound.
pub const LONGEST: usize = 1024 * 1024;

fn rewrite(head: &[u8], headers: &[&str], swaps: &[Swap], out: &mut Vec<u8>) {
    let mut lines = head.split_inclusive(|&byte| byte == b'\n');
    // The request line.
    out.extend_from_slice(lines.next().unwrap_or_default());
    for line in lines {
        let colon = line.iter().position(|&byte| byte == b':');
        let named = colon.filter(|&colon| {
            let field = &line[..colon];
            (headers.iter()).any(|header| header.as_bytes().eq_ignore_ascii_case(field))
        });
        let Some(colon) = named else {
            out.extend_from_slice(line);
            continue;
        };
        let (field, value) = line.split_at(colon + 1);
        out.extend_from_slice(field);
        let basic = field[..colon]
            .eq_ignore_ascii_case(b"authorization")
            .then(|| basic(value, swaps))
            .flatten();
        match basic {
            Some(value) => out.extend_from_slice(&value),
            None => out.extend_from_slice(&replaced(value, swaps)),
        }
    }
}

/// Whether `head`'s one `Host` header names `name`.
fn hosts_name(head: &[u8], name: &str) -> bool {
    let mut fields = [httparse::EMPTY_HEADER; crate::check::HEADERS];
    let mut request = httparse::Request::new(&mut fields);
    if !matches!(request.parse(head), Ok(httparse::Status::Complete(_))) {
        return false;
    }
    let mut hosts = (request.headers.iter())
        .filter(|header| header.name.eq_ignore_ascii_case("host"))
        .map(|header| header.value.trim_ascii());
    match (hosts.next(), hosts.next()) {
        (Some(host), None) => crate::check::host_name(host).is_some_and(|host| {
            let name = name.strip_suffix('.').unwrap_or(name);
            host.eq_ignore_ascii_case(name)
        }),
        _ => false,
    }
}

/// `value`, with each placeholder replaced, unless a value would put a
/// line break in it.
fn replaced(value: &[u8], swaps: &[Swap]) -> Vec<u8> {
    let mut value = value.to_vec();
    for swap in swaps {
        let breaks = swap
            .value
            .iter()
            .any(|&byte| matches!(byte, b'\r' | b'\n' | 0));
        if !breaks && !swap.placeholder.is_empty() {
            value = replace(&value, swap.placeholder, swap.value);
        }
    }
    value
}

/// An `Authorization` header's `value`, after its colon, with the
/// placeholders replaced in its credentials, if it is `Basic`, and its
/// credentials decode.
fn basic(value: &[u8], swaps: &[Swap]) -> Option<Vec<u8>> {
    let start = value
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))?;
    let named = value.get(start..start + 5)?;
    if !named.eq_ignore_ascii_case(b"basic") {
        return None;
    }
    let scheme = &value[start + 5..];
    let spaces = scheme.iter().take_while(|&&byte| byte == b' ').count();
    if spaces == 0 {
        return None;
    }
    let token = &scheme[spaces..];
    let length = token
        .iter()
        .take_while(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .count();
    let (token, rest) = token.split_at(length);
    let credentials = STANDARD.decode(token).ok()?;
    let mut swapped = credentials;
    for swap in swaps {
        if !swap.placeholder.is_empty() {
            swapped = replace(&swapped, swap.placeholder, swap.value);
        }
    }
    let before = value.len() - scheme.len() + spaces;
    let mut out = value[..before].to_vec();
    out.extend_from_slice(STANDARD.encode(swapped).as_bytes());
    out.extend_from_slice(rest);
    Some(out)
}

/// `bytes`, with each `from` replaced by `to`.
fn replace(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(from) {
            out.extend_from_slice(to);
            at += from.len();
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    out
}
