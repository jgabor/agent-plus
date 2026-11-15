//! How the host names each share's device to the guest.

/// The tag of the virtio-fs device that serves the share at `index`, in
/// the order the host lists them. A tag holds at most 36 bytes.
pub fn tag(index: usize) -> String {
    format!("ap-share-{index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_names_the_share_s_place() {
        assert_eq!(tag(0), "ap-share-0");
        assert_eq!(tag(9), "ap-share-9");
        assert!(tag(usize::MAX).len() <= 36);
    }
}
