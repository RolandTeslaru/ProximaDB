//! The social-network test dataset's schema, shared by the measurement binaries.

use proximadb::core::Kind;
use proximadb::load::{LinkTypeSpec, NodeTypeSpec, Schema};

pub fn schema() -> Schema {
    let props = |list: &[(&str, Kind)]| list.iter().map(|(n, k)| (n.to_string(), *k)).collect();
    let node = |name: &str, list: &[(&str, Kind)]| NodeTypeSpec {
        name: name.into(),
        file: format!("nodes_{name}.csv"),
        primary_key: "nodeId".into(),
        properties: props(list),
    };
    let link_type = |name: &str, from: &str, to: &str| LinkTypeSpec {
        name: name.into(),
        from_type: from.into(),
        to_type: to.into(),
        file: format!("edges_{name}__{from}__{to}.csv"),
        from_column: "src".into(),
        to_column: "dst".into(),
    };
    Schema {
        node_types: vec![
            node(
                "User",
                &[
                    ("name", Kind::String),
                    ("status", Kind::String),
                    ("language", Kind::String),
                    ("joined", Kind::Date),
                    ("posts", Kind::Integer),
                    ("sessions", Kind::Integer),
                    ("spent", Kind::Double),
                    ("banned", Kind::Boolean),
                ],
            ),
            node(
                "Device",
                &[
                    ("name", Kind::String),
                    ("os", Kind::String),
                    ("firstSeen", Kind::Date),
                    ("sessions", Kind::Integer),
                ],
            ),
            node("Location", &[("name", Kind::String)]),
        ],
        link_types: vec![
            link_type("FOLLOWS", "User", "User"),
            link_type("USES", "User", "Device"),
            link_type("SEEN_AT", "Device", "Location"),
            link_type("LIVES_IN", "User", "Location"),
        ],
    }
}

#[allow(dead_code)]
pub fn peak_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    // macOS reports bytes, Linux kilobytes.
    if cfg!(target_os = "macos") {
        usage.ru_maxrss as u64
    } else {
        usage.ru_maxrss as u64 * 1024
    }
}
