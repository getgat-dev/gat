//! Deserialize parsed values without requiring an owned intermediate string.

pub(crate) fn parse<'de, D, T, E>(
    deserializer: D,
    parser: fn(&str) -> Result<T, E>,
) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    E: std::fmt::Display,
{
    struct TextParser<T, E>(fn(&str) -> Result<T, E>);

    impl<T, E: std::fmt::Display> serde::de::Visitor<'_> for TextParser<T, E> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a string")
        }

        fn visit_str<Error: serde::de::Error>(self, value: &str) -> Result<T, Error> {
            self.0(value).map_err(Error::custom)
        }
    }

    deserializer.deserialize_str(TextParser(parser))
}

#[cfg(test)]
mod tests {
    use crate::{git::GitCommitId, oid::Oid};

    fn check<T: serde::de::DeserializeOwned + std::fmt::Debug + PartialEq>(raw: &str) {
        let plain = format!("\"{raw}\"");
        let escaped = plain.replacen('a', "\\u0061", 1);
        let expected: T = serde_json::from_str(&plain).unwrap();
        assert_eq!(serde_json::from_str::<T>(&escaped).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<T>(serde_json::Value::String(raw.to_owned())).unwrap(),
            expected
        );
        for invalid in ["null", "123", "[]", "\"not-hex\""] {
            assert!(serde_json::from_str::<T>(invalid).is_err());
        }
        assert!(
            serde_json::from_value::<T>(serde_json::Value::String(raw.to_uppercase())).is_err()
        );
    }

    #[test]
    fn digest_parsers_accept_borrowed_escaped_and_owned_text_without_relaxing_validation() {
        check::<Oid>(&"ab".repeat(32));
        check::<GitCommitId>(&"ab".repeat(20));
        check::<GitCommitId>(&"ab".repeat(32));
    }
}
