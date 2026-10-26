/// Decode only the payload of a UTF-16 console message. Ordinary console
/// lines need no allocation; bridge messages become one owned event string.
pub(crate) fn message_payload_utf16(line: &[u16]) -> Option<String> {
    let prefix = crate::MESSAGE_PREFIX.as_bytes();
    if line.len() < prefix.len()
        || !line
            .iter()
            .zip(prefix)
            .all(|(&unit, &byte)| unit == u16::from(byte))
    {
        return None;
    }
    // CEF rejects malformed UTF-16 rather than posting a lossy replacement.
    String::from_utf16(&line[prefix.len()..]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn recognizes_only_the_complete_prefix_at_the_start() {
        assert_eq!(message_payload_utf16(&[]), None);
        assert_eq!(
            message_payload_utf16(&"gpui-cef:message:".encode_utf16().collect::<Vec<_>>()),
            Some(String::new())
        );
        for line in [
            "gpui-cef:message",
            "xgpui-cef:message:a",
            "ordinary log 日本語",
        ] {
            assert_eq!(
                message_payload_utf16(&line.encode_utf16().collect::<Vec<_>>()),
                None
            );
        }
    }

    #[test]
    fn preserves_unicode_and_nuls_and_rejects_malformed_messages() {
        let payload = "日本語😀\0gpui-cef:message:";
        let line: Vec<u16> = (crate::MESSAGE_PREFIX.to_owned() + payload)
            .encode_utf16()
            .collect();
        assert_eq!(message_payload_utf16(&line), Some(payload.to_owned()));
        for payload in [
            vec![0xd800],
            vec![0xdc00],
            vec![0xd800, 65],
            vec![0xdc00, 0xd800],
        ] {
            let mut line: Vec<u16> = crate::MESSAGE_PREFIX.encode_utf16().collect();
            line.extend(payload);
            assert_eq!(message_payload_utf16(&line), None);
        }
    }

    proptest! {
        #[test]
        fn preserves_arbitrary_unicode_payloads(payload in prop::collection::vec(any::<char>(), 0..4096).prop_map(|chars| chars.into_iter().collect::<String>())) {
            let line: Vec<u16> = (crate::MESSAGE_PREFIX.to_owned() + &payload).encode_utf16().collect();
            prop_assert_eq!(message_payload_utf16(&line), Some(payload));
        }

        #[test]
        fn preserves_valid_payloads_and_rejects_unpaired_surrogates(payload in prop::collection::vec(any::<u16>(), 0..4096)) {
            let mut line: Vec<u16> = crate::MESSAGE_PREFIX.encode_utf16().collect();
            line.extend_from_slice(&payload);
            prop_assert_eq!(message_payload_utf16(&line), String::from_utf16(&payload).ok());
        }

        #[test]
        fn matches_the_string_protocol(line in prop::collection::vec(any::<char>(), 0..4096).prop_map(|chars| chars.into_iter().collect::<String>())) {
            prop_assert_eq!(message_payload_utf16(&line.encode_utf16().collect::<Vec<_>>()), crate::message_payload(&line).map(str::to_owned));
        }
    }
}
