//! The heap memory a dataset holds, as `/$/stats`, `/$/metrics` and the memory quota count it
//! (#392): its engine's parts ([`tripletrail::MemoryUse`]) and the map's caches the server
//! keeps for it (the collections, tiles, facet counts and their labels).

use serde_json::Value;

use crate::admin::Entry;

/// What a dataset holds on the heap, by part.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Usage {
    pub(crate) engine: tripletrail::MemoryUse,
    /// The map's and the OGC API's caches of this dataset.
    pub(crate) map_caches: usize,
}

impl Usage {
    pub(crate) fn total(&self) -> usize {
        self.engine.total() + self.map_caches
    }

    /// The parts by name, as `/$/stats` and `/$/metrics` show them.
    pub(crate) fn parts(&self) -> Vec<(&'static str, usize)> {
        let mut parts = self.engine.parts().to_vec();
        parts.push(("mapCaches", self.map_caches));
        parts
    }
}

/// The dataset's memory; `None` on Oxigraph's store, which does not count it.
pub(crate) fn of(entry: &Entry) -> Option<Usage> {
    let engine = entry.engine.memory_use()?;
    Some(Usage {
        engine,
        map_caches: crate::tiles::cache_bytes(entry.id) + crate::ogc::cache_bytes(entry.id),
    })
}

/// Roughly the heap bytes of a JSON value: its strings, arrays and objects (a map entry with
/// its key, value and a share of tree node).
pub(crate) fn json_bytes(v: &Value) -> usize {
    const ENTRY: usize = size_of::<String>() + size_of::<Value>() + 16;
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
        Value::String(s) => s.capacity(),
        Value::Array(a) => {
            a.capacity() * size_of::<Value>() + a.iter().map(json_bytes).sum::<usize>()
        }
        Value::Object(o) => o
            .iter()
            .map(|(k, v)| ENTRY + k.capacity() + json_bytes(v))
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::json_bytes;
    use serde_json::json;

    #[test]
    fn json_bytes_count_strings_arrays_and_objects() {
        assert_eq!(json_bytes(&json!(null)), 0);
        assert_eq!(json_bytes(&json!(1.5)), 0);
        assert_eq!(json_bytes(&json!("abcd")), 4);
        let array = json!(["ab", "cd"]);
        assert!(json_bytes(&array) >= 2 * size_of::<serde_json::Value>() + 4);
        let object = json!({"key": "value", "n": [1, 2, 3]});
        let small = json!({"key": "value"});
        assert!(json_bytes(&object) > json_bytes(&small));
        assert!(json_bytes(&small) >= "key".len() + "value".len());
    }
}
