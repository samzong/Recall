use super::*;

#[test]
fn remote_parent_identities_follow_keys_across_unknown_relations() {
    crate::db::schema::register_sqlite_vec();
    let source = Store::open_in_memory().unwrap();
    let id = seed(&source, "parent identity contract", true);
    let template = source.remote_snapshot(source.get_session_by_id(&id).unwrap().unwrap()).unwrap();
    let destination = Store::open_in_memory().unwrap();
    let mut cloud = BTreeMap::new();
    let mut parents = Vec::new();
    let mut identities = Vec::new();
    let keys = [
        ("spawn", "codex", "shared"),
        ("fork", "codex", "shared"),
        ("spawn", "other", "shared"),
        ("spawn", "codex", "other"),
    ];
    for (index, (relation, source, source_id)) in keys.into_iter().enumerate() {
        if index % 2 == 0 {
            parents.push(serde_json::json!({
                "relation": "future-relation", "source": "codex", "source_id": "shared"
            }));
            identities.push(Some(uuid::Uuid::new_v4().to_string()));
        }
        let mut record = template.clone();
        record["session"]["source"] = source.into();
        record["session"]["source_id"] = source_id.into();
        record["session"]["title"] = format!("parent-{index}").into();
        let revision = Revision {
            version: 1,
            sync_id: uuid::Uuid::new_v4().to_string(),
            parent: None,
            parent_sessions: Vec::new(),
            record,
        };
        parents.push(serde_json::json!({
            "relation": relation, "source": source, "source_id": source_id
        }));
        identities.push(Some(revision.sync_id.clone()));
        let body = serde_json::to_vec(&revision).unwrap();
        cloud.insert(format!("v1/revisions/{}.json", digest(&body)), body);
    }
    parents.push(serde_json::json!({
        "relation": "resume", "source": "codex", "source_id": "unresolved"
    }));
    identities.push(None);
    let mut record = template;
    record["session"]["source_id"] = "child".into();
    record["session"]["topology"]["parents"] = parents.into();
    let revision = Revision {
        version: 1,
        sync_id: uuid::Uuid::new_v4().to_string(),
        parent: None,
        parent_sessions: identities,
        record,
    };
    let body = serde_json::to_vec(&revision).unwrap();
    cloud.insert(format!("v1/revisions/{}.json", digest(&body)), body);
    assert_eq!(transfer(&destination, &mut cloud).unwrap().downloaded, 5);
    let child = destination.get_session_by_source_id("codex", "child").unwrap().unwrap();
    let topology = destination.session_topology(&child.id).unwrap();
    assert_eq!(topology.parents.len(), 5);
    for (index, (relation, source, source_id)) in keys.into_iter().enumerate() {
        let parent = crate::types::ParentLink {
            relation: relation.parse().unwrap(),
            source: source.into(),
            source_id: source_id.into(),
        };
        let resolved = destination.resolve_parent(&child.id, &parent).unwrap().unwrap();
        assert_eq!(resolved.title, format!("parent-{index}"));
    }
    let unresolved =
        topology.parents.iter().find(|parent| parent.source_id == "unresolved").unwrap();
    assert!(destination.resolve_parent(&child.id, unresolved).unwrap().is_none());
    assert_eq!(transfer(&destination, &mut cloud).unwrap().uploaded, 0);
}
