use serde_json::Value;

use crate::{Error, Result};

pub const PAGE_SIZE: usize = 100;

pub fn collect_pages<F>(base_endpoint: &str, mut fetch: F) -> Result<(Vec<Value>, u64)>
where
    F: FnMut(&str) -> Result<Value>,
{
    let mut all = Vec::new();
    let mut page = 1_u64;
    loop {
        let separator = if base_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let endpoint = format!("{base_endpoint}{separator}per_page={PAGE_SIZE}&page={page}");
        let value = fetch(&endpoint)?;
        let items = value.as_array().ok_or_else(|| Error::Json {
            context: "paginated collection".to_owned(),
        })?;
        let count = items.len();
        all.extend(items.iter().cloned());
        if count < PAGE_SIZE {
            return Ok((all, page));
        }
        page += 1;
        if page > 100_000 {
            return Err(Error::Remote("pagination exceeded safety limit".to_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_zero_one_and_multiple_pages() {
        let (items, pages) = collect_pages("x", |_| Ok(Value::Array(vec![]))).unwrap();
        assert!(items.is_empty());
        assert_eq!(pages, 1);

        let (items, pages) = collect_pages("x", |_| Ok(Value::Array(vec![Value::Null]))).unwrap();
        assert_eq!((items.len(), pages), (1, 1));

        let mut calls = 0;
        let (items, pages) = collect_pages("x", |_| {
            calls += 1;
            Ok(Value::Array(if calls == 1 {
                vec![Value::Null; PAGE_SIZE]
            } else {
                vec![Value::Null]
            }))
        })
        .unwrap();
        assert_eq!((items.len(), pages), (PAGE_SIZE + 1, 2));
    }
}
