use super::*;

fn revision_keys(cloud: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    cloud.keys().filter(|key| key.starts_with("v1/revisions/")).cloned().collect()
}

fn edit(store: &Store, id: &str, text: &str) {
    store
        .conn
        .execute("UPDATE messages SET content = ?1 WHERE session_id = ?2", [text, id])
        .unwrap();
}

#[test]
fn remote_object_names_reject_malformed_encodings() {
    let hash = "a".repeat(64);
    let other = "b".repeat(64);
    for key in [
        format!("v1/revisions/{hash}.json"),
        format!("v1/revisions/{hash}.json.zst"),
        format!("v1/revisions/{hash}.delta-{other}.zst"),
        format!("v1/metadata/{hash}.json"),
    ] {
        assert_eq!(
            object::Name::parse(&key).unwrap().canonical,
            key.split_once(['.']).unwrap().0.to_string() + ".json"
        );
    }
    for key in [
        format!("v1/revisions/{hash}.zst"),
        format!("v1/revisions/{hash}.delta-{hash}.zst"),
        format!("v1/revisions/{hash}.delta-xyz.zst"),
        format!("v1/metadata/{hash}.json.zst"),
        format!("v1/revisions/{hash}.json.gz"),
    ] {
        assert!(object::Name::parse(&key).is_err(), "{key}");
    }
}

#[test]
fn remote_revisions_upload_compressed_parent_deltas_and_decode_out_of_order() {
    crate::db::schema::register_sqlite_vec();
    let a = Store::open_in_memory().unwrap();
    let id = seed(&a, &"compressible synthetic message ".repeat(512), true);
    let mut cloud = BTreeMap::new();
    transfer(&a, &mut cloud).unwrap();
    let [base] = revision_keys(&cloud).try_into().unwrap();
    assert!(base.ends_with(".json.zst"));
    assert!(
        cloud
            .keys()
            .filter(|key| key.starts_with("v1/metadata/"))
            .all(|key| key.ends_with(".json"))
    );
    edit(&a, &id, &"updated synthetic message ".repeat(512));
    assert_eq!(transfer(&a, &mut cloud).unwrap().uploaded, 1);
    let base_hash = object::Name::parse(&base).unwrap().canonical;
    let base_hash = base_hash.trim_start_matches("v1/revisions/").trim_end_matches(".json");
    let delta = revision_keys(&cloud).into_iter().find(|key| *key != base).unwrap();
    assert!(delta.ends_with(&format!(".delta-{base_hash}.zst")), "{delta}");
    let raw = raw_objects(&cloud);
    let canonical = object::Name::parse(&delta).unwrap().canonical;
    assert!(cloud[&delta].len() * 4 < zstd::encode_all(&raw[&canonical][..], 3).unwrap().len());
    assert!(
        raw.values().map(Vec::len).sum::<usize>()
            > cloud.values().map(Vec::len).sum::<usize>() * 10
    );

    let b = Store::open_in_memory().unwrap();
    let summary = transfer_observing(&b, &mut cloud, &|key, _| {
        if key == base {
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    })
    .unwrap();
    assert_eq!((summary.downloaded, summary.uploaded), (cloud.len(), 0));
    let session = b.list_recent_sessions(1).unwrap().pop().unwrap();
    assert!(b.get_messages(&session.id).unwrap()[0].content.starts_with("updated synthetic"));
    for (key, body) in &raw {
        assert_eq!(b.cached_remote(key).unwrap().as_ref(), Some(body));
        assert_eq!(b.cached_remote_size(key).unwrap(), Some(body.len() as u64));
        assert!(b.cached_stored(key).unwrap().unwrap().starts_with(&[0x28, 0xb5, 0x2f, 0xfd]));
    }
    assert_eq!(
        b.cached_stored(&object::Name::parse(&base).unwrap().canonical).unwrap().as_ref(),
        Some(&cloud[&base])
    );
    let summary = transfer(&b, &mut cloud).unwrap();
    assert_eq!((summary.downloaded, summary.uploaded), (0, 0));
}

#[test]
fn remote_delta_without_its_base_waits_until_the_base_is_restored() {
    crate::db::schema::register_sqlite_vec();
    let a = Store::open_in_memory().unwrap();
    let id = seed(&a, &"delta base message ".repeat(512), true);
    let mut cloud = BTreeMap::new();
    transfer(&a, &mut cloud).unwrap();
    let [base] = revision_keys(&cloud).try_into().unwrap();
    edit(&a, &id, &"delta child message ".repeat(512));
    transfer(&a, &mut cloud).unwrap();
    let lost = cloud.remove(&base).unwrap();
    assert!(revision_keys(&cloud).iter().all(|key| key.contains(".delta-")));

    let c = Store::open_in_memory().unwrap();
    let summary = transfer(&c, &mut cloud).unwrap();
    assert_eq!((summary.downloaded, summary.uploaded), (cloud.len() - 1, 0));
    assert_eq!(c.stats().unwrap().0, 0);

    assert_eq!(transfer(&a, &mut cloud).unwrap().uploaded, 1);
    assert_eq!(cloud[&base], lost);
    assert_eq!(transfer(&c, &mut cloud).unwrap().downloaded, 2);
    let session = c.list_recent_sessions(1).unwrap().pop().unwrap();
    assert!(c.get_messages(&session.id).unwrap()[0].content.starts_with("delta child"));
}

#[test]
fn remote_legacy_raw_objects_and_duplicate_encodings_resolve_to_one_revision() {
    crate::db::schema::register_sqlite_vec();
    let a = Store::open_in_memory().unwrap();
    let id = seed(&a, &"legacy interop message ".repeat(512), true);
    let mut cloud = BTreeMap::new();
    transfer(&a, &mut cloud).unwrap();
    edit(&a, &id, &"legacy interop update ".repeat(512));
    transfer(&a, &mut cloud).unwrap();
    let raw = raw_objects(&cloud);
    let mut mixed = cloud.clone();
    mixed.extend(raw.iter().map(|(key, body)| (key.clone(), body.clone())));

    let b = Store::open_in_memory().unwrap();
    let summary = transfer(&b, &mut mixed).unwrap();
    assert_eq!((summary.downloaded, summary.uploaded), (raw.len(), 0));
    let session = b.list_recent_sessions(1).unwrap().pop().unwrap();
    assert!(b.get_messages(&session.id).unwrap()[0].content.starts_with("legacy interop update"));
    for (key, body) in &raw {
        assert_eq!(b.cached_remote(key).unwrap().as_ref(), Some(body));
    }

    let mut legacy_cloud: BTreeMap<_, _> = raw.clone();
    let legacy = Store::open_in_memory().unwrap();
    for (key, body) in &raw {
        let hash = key.trim_end_matches(".json").rsplit_once('/').unwrap().1;
        if key.starts_with("v1/revisions/") {
            let revision: Revision = serde_json::from_slice(body).unwrap();
            legacy
                .conn
                .execute(
                    "INSERT INTO sync_revisions(digest, sync_id, body) VALUES (?1, ?2, ?3)",
                    rusqlite::params![hash, revision.sync_id, body],
                )
                .unwrap();
        } else {
            legacy
                .conn
                .execute(
                    "INSERT INTO sync_objects(digest, body) VALUES (?1, ?2)",
                    rusqlite::params![hash, body],
                )
                .unwrap();
        }
        assert_eq!(legacy.cached_remote(key).unwrap().as_ref(), Some(body));
        assert_eq!(legacy.cached_remote_size(key).unwrap(), Some(body.len() as u64));
    }
    let summary = transfer(&legacy, &mut legacy_cloud).unwrap();
    assert_eq!((summary.downloaded, summary.uploaded), (0, 0));
    let session = legacy.list_recent_sessions(10).unwrap().pop().unwrap();
    assert!(
        legacy.get_messages(&session.id).unwrap()[0].content.starts_with("legacy interop update")
    );

    let fresh = Store::open_in_memory().unwrap();
    transfer(&fresh, &mut legacy_cloud).unwrap();
    for key in raw.keys().filter(|key| key.starts_with("v1/revisions/")) {
        assert!(fresh.cached_stored(key).unwrap().unwrap().starts_with(&[0x28, 0xb5, 0x2f, 0xfd]));
    }
    legacy_cloud.retain(|key, _| !key.starts_with("v1/revisions/"));
    assert_eq!(transfer(&fresh, &mut legacy_cloud).unwrap().uploaded, 2);
    assert!(revision_keys(&legacy_cloud).iter().any(|key| key.contains(".delta-")));
    assert_eq!(raw_objects(&legacy_cloud), raw);
}
