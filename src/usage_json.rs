use crate::compression;

pub(crate) const IDENTITY: &str = "identity";
pub(crate) const GZIP: &str = "gzip";

pub(crate) fn encode(json: &str) -> (Vec<u8>, &'static str) {
    compression::gzip_if_smaller(json.as_bytes())
}

pub(crate) fn decode(bytes: &[u8], encoding: &str) -> Option<String> {
    let decoded = match encoding {
        GZIP => compression::decode_gzip(bytes)?,
        _ => bytes.to_vec(),
    };
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
pub(crate) async fn load(
    database: &impl sea_orm::ConnectionTrait,
    request_id: &str,
) -> Result<Option<String>, sea_orm::DbErr> {
    let Some(row) = database
        .query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DbBackend::Sqlite,
            "SELECT raw_usage_json, raw_usage_encoding FROM gateway_usage WHERE request_id = ?",
            [request_id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let bytes: Option<Vec<u8>> = row.try_get("", "raw_usage_json")?;
    let encoding: String = row.try_get("", "raw_usage_encoding")?;
    Ok(bytes.and_then(|bytes| decode(&bytes, &encoding)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_object_round_trips_through_gzip() {
        let json = format!(r#"{{"attribution":"{}"}}"#, "x".repeat(4096));
        let (bytes, encoding) = encode(&json);
        assert_eq!(encoding, GZIP);
        assert!(bytes.len() < json.len());
        assert_eq!(decode(&bytes, encoding).as_deref(), Some(json.as_str()));
    }

    #[test]
    fn a_tiny_object_stays_identity() {
        let (bytes, encoding) = encode("{}");
        assert_eq!(encoding, IDENTITY);
        assert_eq!(bytes, b"{}");
        assert_eq!(decode(&bytes, encoding).as_deref(), Some("{}"));
    }

    #[test]
    fn corrupt_gzip_decodes_to_nothing() {
        assert_eq!(decode(b"not gzip", GZIP), None);
    }
}
