//! Crawling the server's caches (foreverdb `docs/cache-crawler.md`).
//!
//! Items the Forever client carries encrypted and the texts of quests are only known to the
//! game server; it answers a client that asks into `Cache/ADB/<locale>/DBCache.bin` and
//! `Cache/WDB/<locale>/questcache.wdb`. For an uploader an admin made a crawler, the API lists
//! what is still missing for the client's build. The watcher writes that list into the local
//! addon `ForeverCollect_CacheIDs`, where `/fc itemcache` and `/fc questcache` of ForeverCollect
//! read it, and uploads the two caches once the game has closed and they changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tauri::AppHandle;

use crate::{
    active_clients, api_url, content_hash, emit_activity, gzip, notify, target_url, tokio_sleep,
    AppState, ClientStatus, CLIENT_TAG, CONNECT_TIMEOUT, CONTENT_HASH_HEADER, USER_HEADER,
};
use tauri::Manager;

/// Only Forever has encrypted content; Classic Era needs no crawl (assumption A7).
const CRAWLED_PRODUCT: &str = "wowf";
/// The API context and game_products slug of that product.
const CONTEXT: &str = "forever";
pub(crate) const ADDON: &str = "ForeverCollect_CacheIDs";
const QUEST_FILE: &str = "QuestIDs.lua";
const ITEM_FILE: &str = "ItemIDs.lua";
/// How often the crawler check and the list are refreshed; an import shortens the list.
const LIST_REFRESH: Duration = Duration::from_secs(15 * 60);
/// How often the watcher looks at the crawl state.
pub(crate) const CRAWL_INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CrawlList {
    pub build: u32,
    pub interface_version: u32,
    pub items: Vec<u32>,
    pub quests: Vec<u32>,
}

/// What the watcher knows about a client: which identity crawls for it and when it asked.
#[derive(Clone, Default)]
pub(crate) struct ClientCrawl {
    build: Option<u32>,
    crawler: Option<String>,
    checked: Option<Instant>,
}

/// The newest DB2 hotfix push of the product the worker found on wago.tools.
#[derive(Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HotfixPush {
    pub build: u32,
    pub push_id: u32,
}

#[derive(Deserialize, Debug, PartialEq)]
struct HotfixAnswer {
    push: Option<HotfixPush>,
}

/// Whether a push needs a notification and whether to remember it. The first push the app
/// learns of is only remembered, so a new crawler is not greeted with an old hotfix.
pub(crate) fn hotfix_notice(notified: Option<u32>, push_id: u32) -> Option<bool> {
    match notified {
        None => Some(false),
        Some(known) if push_id > known => Some(true),
        Some(_) => None,
    }
}

enum Answer {
    List(CrawlList),
    NoCrawler,
    NotImported,
}

/// "1.60.1.70707" -> 70707.
pub(crate) fn build_number(version: &str) -> Option<u32> {
    version.rsplit('.').next()?.parse().ok()
}

/// One data file of the addon, in the format foreverdb-import writes: the sorted ids, 20 per
/// line, in a global table ForeverCollect reads from another addon. `.build` names the build
/// the list is for, so ForeverCollect can tell a list of the previous build after an update.
pub(crate) fn id_list_lua(key: &str, ids: &[u32], build: u32) -> String {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut out = format!(
        "-- written by the ForeverDB uploader from the crawl list of build {build}; do not edit\n\
         ForeverCollectCacheIDs = ForeverCollectCacheIDs or {{}}\n\
         ForeverCollectCacheIDs.build = {build}\n\nForeverCollectCacheIDs.{key} = {{\n"
    );
    for chunk in sorted.chunks(20) {
        let line: Vec<String> = chunk.iter().map(u32::to_string).collect();
        out.push_str(&format!("    {},\n", line.join(", ")));
    }
    out.push_str("}\n");
    out
}

pub(crate) fn toc(interface_version: u32) -> String {
    format!(
        "## Interface: {interface_version}\n\
         ## Title: ForeverCollect cache IDs\n\
         ## Notes: ID lists for /fc questcache and /fc itemcache, written by the ForeverDB uploader for crawlers.\n\
         ## RequiredDeps: ForeverCollect\n\n\
         {QUEST_FILE}\n{ITEM_FILE}\n"
    )
}

/// Writes the list into the client's AddOns folder; returns whether a file changed.
pub(crate) fn write_list(client_dir: &Path, list: &CrawlList) -> std::io::Result<bool> {
    let dir = client_dir.join("Interface").join("AddOns").join(ADDON);
    fs::create_dir_all(&dir)?;
    let files = [
        (format!("{ADDON}.toc"), toc(list.interface_version)),
        (
            QUEST_FILE.to_string(),
            id_list_lua("quests", &list.quests, list.build),
        ),
        (
            ITEM_FILE.to_string(),
            id_list_lua("items", &list.items, list.build),
        ),
    ];
    let mut changed = false;
    for (name, content) in files {
        let path = dir.join(name);
        if fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
            continue;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, content)?;
        fs::rename(&tmp, &path)?;
        changed = true;
    }
    Ok(changed)
}

/// Removes the addon from the client's AddOns folder; returns whether there was one. Only
/// approved crawlers may have it: ForeverCollect shows the crawl commands while it is there.
pub(crate) fn remove_list(client_dir: &Path) -> std::io::Result<bool> {
    let dir = client_dir.join("Interface").join("AddOns").join(ADDON);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// How the API answered one identity of a client during a refresh.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Reply {
    List,
    NoCrawler,
    NotImported,
    Error,
}

/// Whether a refresh says the client has no crawler anymore: every identity asked was turned
/// down (403). A build not imported yet or an error says nothing about the approval.
fn approval_revoked(replies: &[Reply]) -> bool {
    !replies.is_empty() && replies.iter().all(|reply| *reply == Reply::NoCrawler)
}

/// A client cache file worth uploading.
#[derive(Debug, PartialEq)]
pub(crate) struct CacheFile {
    pub kind: &'static str,
    pub locale: String,
    pub path: PathBuf,
}

/// Magic and offset of the build in the header, as the ingress checks them.
fn cache_header(kind: &str) -> (&'static [u8], usize) {
    match kind {
        "dbcache" => (b"XFTH", 8),
        _ => (b"TSQW", 4),
    }
}

/// The client's caches of the build, per locale: `Cache/ADB/<locale>/DBCache.bin` and
/// `Cache/WDB/<locale>/questcache.wdb`. Files of another build (the client empties its caches
/// on a build change, but only when it starts) are left out.
pub(crate) fn cache_files(client_dir: &Path, build: u32) -> Vec<CacheFile> {
    let mut files = Vec::new();
    for (kind, dir, name) in [
        ("dbcache", "ADB", "DBCache.bin"),
        ("questcache", "WDB", "questcache.wdb"),
    ] {
        let Ok(locales) = fs::read_dir(client_dir.join("Cache").join(dir)) else {
            continue;
        };
        for entry in locales.flatten() {
            let locale = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path().join(name);
            if locale.len() == 4 && cache_build(&path, kind) == Some(build) {
                files.push(CacheFile { kind, locale, path });
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn cache_build(path: &Path, kind: &str) -> Option<u32> {
    let (magic, offset) = cache_header(kind);
    let mut header = vec![0u8; offset + 4];
    use std::io::Read;
    fs::File::open(path).ok()?.read_exact(&mut header).ok()?;
    if !header.starts_with(magic) {
        return None;
    }
    Some(u32::from_le_bytes(
        header[offset..offset + 4].try_into().ok()?,
    ))
}

/// The ingress route next to the snapshot upload: `.../imports/forevercollect` ->
/// `.../imports/client-cache`.
pub(crate) fn cache_upload_url(snapshot_url: &str) -> String {
    let base = snapshot_url.trim_end_matches('/');
    let base = base.strip_suffix("/forevercollect").unwrap_or(base);
    format!("{base}/client-cache")
}

fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
        .map_err(|e| format!("Could not create the HTTP client: {e}"))
}

async fn fetch_list(user_id: &str, build: u32) -> Result<Answer, String> {
    let response = http_client(REQUEST_TIMEOUT)?
        .get(format!(
            "{}/v1/{CONTEXT}/crawl",
            api_url().trim_end_matches('/')
        ))
        .query(&[("build", build)])
        .header("X-ForeverDB-Client", CLIENT_TAG)
        .header(USER_HEADER, user_id)
        .send()
        .await
        .map_err(|e| format!("Could not fetch the crawl list: {e}"))?;
    match response.status() {
        status if status.is_success() => response
            .json::<CrawlList>()
            .await
            .map(Answer::List)
            .map_err(|e| format!("Could not read the crawl list: {e}")),
        reqwest::StatusCode::FORBIDDEN => Ok(Answer::NoCrawler),
        reqwest::StatusCode::CONFLICT => Ok(Answer::NotImported),
        status => Err(format!("The crawl list answered {status}.")),
    }
}

async fn fetch_hotfix(user_id: &str) -> Result<Option<HotfixPush>, String> {
    let response = http_client(REQUEST_TIMEOUT)?
        .get(format!(
            "{}/v1/{CONTEXT}/crawl/hotfix",
            api_url().trim_end_matches('/')
        ))
        .header("X-ForeverDB-Client", CLIENT_TAG)
        .header(USER_HEADER, user_id)
        .send()
        .await
        .map_err(|e| format!("Could not fetch the latest hotfix: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("The hotfix check answered {}.", response.status()));
    }
    response
        .json::<HotfixAnswer>()
        .await
        .map(|answer| answer.push)
        .map_err(|e| format!("Could not read the latest hotfix: {e}"))
}

/// Tells the crawler once per hotfix push that the game has new records to ask the server for.
async fn check_hotfix(app: &AppHandle, state: &AppState, user: &str) {
    let push = match fetch_hotfix(user).await {
        Ok(Some(push)) => push,
        Ok(None) => return,
        Err(error) => {
            emit_activity(app, "", None, "error", error, None);
            return;
        }
    };
    let Some(show) = hotfix_notice(state.snapshot().notified_hotfix_push, push.push_id) else {
        return;
    };
    state.update(|settings| settings.notified_hotfix_push = Some(push.push_id));
    if !show {
        return;
    }
    let message = format!(
        "New hotfix for Forever build {}: log in and run /fc itemcache and /fc questcache.",
        push.build
    );
    emit_activity(app, "", None, "crawl", message, None);
    notify(
        app,
        &format!("New hotfix for Forever build {}", push.build),
        "Log in and run /fc itemcache and /fc questcache, then close the game.",
    );
}

async fn upload_cache(
    user_id: &str,
    build: u32,
    file: &CacheFile,
    content: &[u8],
) -> Result<(), String> {
    let compressed = gzip(content).map_err(|e| format!("Could not prepare the upload: {e}"))?;
    let response = http_client(UPLOAD_TIMEOUT)?
        .post(cache_upload_url(&target_url()))
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(reqwest::header::CONTENT_ENCODING, "gzip")
        .header("X-ForeverDB-Client", CLIENT_TAG)
        .header(USER_HEADER, user_id)
        .header(CONTENT_HASH_HEADER, content_hash(&compressed))
        .header("X-ForeverDB-Cache-Kind", file.kind)
        .header("X-ForeverDB-Product", CONTEXT)
        .header("X-ForeverDB-Build", build.to_string())
        .header("X-ForeverDB-Locale", &file.locale)
        .body(compressed)
        .send()
        .await
        .map_err(|e| format!("Could not upload {}: {e}", file.path.display()))?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    Err(format!(
        "Upload of {} was refused ({status}): {}",
        file.path.display(),
        text.trim()
    ))
}

/// Runs for the life of the app, like the SavedVariables watcher; independent of the automatic
/// upload switch, since only crawlers an admin set up get a list.
pub(crate) async fn watch(app: AppHandle) {
    loop {
        tokio_sleep(CRAWL_INTERVAL).await;
        let state = app.state::<AppState>();
        let (_, _, clients) = active_clients(&state.snapshot(), None);
        state.ensure_identities(&clients);
        for client in &clients {
            watch_client(&app, &state, client).await;
        }
    }
}

/// One round of the crawl watcher for a client: find the crawler identity and refresh the
/// list now and then, and upload changed caches once the game is closed.
pub(crate) async fn watch_client(app: &AppHandle, state: &AppState, client: &ClientStatus) {
    if client.product != CRAWLED_PRODUCT {
        return;
    }
    let Some(build) = client.version.as_deref().and_then(build_number) else {
        return;
    };
    let known = state
        .crawl
        .lock()
        .unwrap()
        .get(&client.id)
        .cloned()
        .unwrap_or_default();
    let due = known.build != Some(build)
        || known
            .checked
            .is_none_or(|checked| checked.elapsed() >= LIST_REFRESH);
    let mut crawler = if known.build == Some(build) {
        known.crawler.clone()
    } else {
        None
    };
    if due {
        crawler = None;
        let mut users: Vec<String> = client
            .accounts
            .iter()
            .map(|account| state.user_id(&account.bnet_id))
            .collect();
        users.sort();
        users.dedup();
        let mut replies = Vec::new();
        for user in users {
            match fetch_list(&user, build).await {
                Ok(Answer::List(list)) => {
                    replies.push(Reply::List);
                    crawler = Some(user);
                    write_crawl_list(app, client, &list);
                    break;
                }
                Ok(Answer::NoCrawler) => replies.push(Reply::NoCrawler),
                Ok(Answer::NotImported) => {
                    // the server has no catalog of the build yet; ask again with the next round
                    replies.push(Reply::NotImported);
                    crawler = Some(user);
                    break;
                }
                Err(error) => {
                    replies.push(Reply::Error);
                    // reported once per refresh, not with every round of the watcher
                    emit_activity(
                        app,
                        &client.id,
                        None,
                        "error",
                        format!("{}: {error}", client.label),
                        None,
                    );
                    break;
                }
            }
        }
        if crawler.is_none() && approval_revoked(&replies) {
            remove_crawl_list(app, client);
        }
        if let Some(user) = &crawler {
            check_hotfix(app, state, user).await;
        }
        state.crawl.lock().unwrap().insert(
            client.id.clone(),
            ClientCrawl {
                build: Some(build),
                crawler: crawler.clone(),
                checked: Some(Instant::now()),
            },
        );
    }
    // the game writes its caches while it runs; upload what a closed client left
    let Some(user) = crawler else { return };
    if client.running {
        return;
    }
    for file in cache_files(Path::new(&client.id), build) {
        let Ok(content) = fs::read(&file.path) else {
            continue;
        };
        let hash = content_hash(&content);
        let key = file.path.to_string_lossy().into_owned();
        if state.snapshot().crawl_upload_hash.get(&key) == Some(&hash) {
            continue;
        }
        match upload_cache(&user, build, &file, &content).await {
            Ok(()) => {
                state.update(|settings| {
                    settings.crawl_upload_hash.insert(key, hash);
                });
                emit_activity(
                    app,
                    &client.id,
                    None,
                    "crawl",
                    format!(
                        "{}: {} ({}) of build {build} uploaded for the crawl.",
                        client.label, file.kind, file.locale
                    ),
                    None,
                );
                // the next import answers part of the list: ask for it again soon
                if let Some(entry) = state.crawl.lock().unwrap().get_mut(&client.id) {
                    entry.checked = None;
                }
            }
            Err(error) => emit_activity(
                app,
                &client.id,
                None,
                "error",
                format!("{}: {error}", client.label),
                None,
            ),
        }
    }
}

/// Removes the addon of a client whose crawler approval is gone, also while WoW runs: the
/// addon disappears with the next /reload or login.
fn remove_crawl_list(app: &AppHandle, client: &ClientStatus) {
    match remove_list(Path::new(&client.id)) {
        Ok(false) => {}
        Ok(true) => emit_activity(
            app,
            &client.id,
            None,
            "crawl",
            format!(
                "{}: crawl list removed (not a crawler anymore).",
                client.label
            ),
            None,
        ),
        Err(error) => emit_activity(
            app,
            &client.id,
            None,
            "error",
            format!("{}: could not remove the crawl list: {error}", client.label),
            None,
        ),
    }
}

fn write_crawl_list(app: &AppHandle, client: &ClientStatus, list: &CrawlList) {
    match write_list(Path::new(&client.id), list) {
        Ok(false) => {}
        Ok(true) => emit_activity(
            app,
            &client.id,
            None,
            "crawl",
            format!(
                "{}: crawl list of build {} written ({} items, {} quests).{}",
                client.label,
                list.build,
                list.items.len(),
                list.quests.len(),
                if client.running {
                    " WoW is running: /reload, then /fc itemcache and /fc questcache."
                } else {
                    " In the game: /fc itemcache and /fc questcache, then log out."
                }
            ),
            None,
        ),
        Err(error) => emit_activity(
            app,
            &client.id,
            None,
            "error",
            format!("{}: could not write the crawl list: {error}", client.label),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("foreverdb-crawl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_first_hotfix_is_remembered_and_only_newer_ones_are_notified() {
        assert_eq!(hotfix_notice(None, 17058516), Some(false));
        assert_eq!(hotfix_notice(Some(17056256), 17058516), Some(true));
        assert_eq!(hotfix_notice(Some(17058516), 17058516), None);
        assert_eq!(hotfix_notice(Some(17058516), 17056256), None);
    }

    #[test]
    fn the_hotfix_answer_has_the_format_the_api_sends() {
        let answer: HotfixAnswer = serde_json::from_str(
            r#"{"push":{"build":70707,"pushId":17058516,"pushedAt":"2026-10-31T18:02:11.000Z"}}"#,
        )
        .unwrap();
        assert_eq!(
            answer.push,
            Some(HotfixPush {
                build: 70707,
                push_id: 17058516
            })
        );
        let none: HotfixAnswer = serde_json::from_str(r#"{"push":null}"#).unwrap();
        assert_eq!(none.push, None);
    }

    #[test]
    fn build_number_is_the_last_part_of_the_version() {
        assert_eq!(build_number("1.60.1.70707"), Some(70707));
        assert_eq!(build_number("1.60.1"), Some(1));
        assert_eq!(build_number("x"), None);
    }

    #[test]
    fn the_lists_have_the_format_foreverdb_import_writes() {
        let ids: Vec<u32> = (1..=21).rev().collect();
        let lua = id_list_lua("items", &ids, 70707);
        assert!(lua.contains("ForeverCollectCacheIDs = ForeverCollectCacheIDs or {}\n"));
        assert!(lua.contains("ForeverCollectCacheIDs.build = 70707\n"));
        assert!(lua.contains(
            "ForeverCollectCacheIDs.items = {\n    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,\n    21,\n}\n"
        ));
        assert!(toc(16001).starts_with("## Interface: 16001\n"));
    }

    #[test]
    fn removing_the_list_deletes_the_addon_folder() {
        let dir = temp_dir("remove");
        let list = CrawlList {
            build: 70707,
            interface_version: 16001,
            items: vec![1],
            quests: vec![2],
        };
        write_list(&dir, &list).unwrap();
        assert!(remove_list(&dir).unwrap());
        assert!(!dir.join("Interface/AddOns").join(ADDON).exists());
        // nothing left to remove
        assert!(!remove_list(&dir).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_refusal_of_every_identity_revokes_the_approval() {
        assert!(approval_revoked(&[Reply::NoCrawler]));
        assert!(approval_revoked(&[Reply::NoCrawler, Reply::NoCrawler]));
        // a build not imported yet or an error leaves the addon alone
        assert!(!approval_revoked(&[Reply::NoCrawler, Reply::NotImported]));
        assert!(!approval_revoked(&[Reply::NoCrawler, Reply::Error]));
        assert!(!approval_revoked(&[Reply::Error]));
        assert!(!approval_revoked(&[Reply::List]));
        // a client without accounts asked nobody
        assert!(!approval_revoked(&[]));
    }

    #[test]
    fn writing_the_same_list_twice_changes_nothing() {
        let dir = temp_dir("list");
        let list = CrawlList {
            build: 70707,
            interface_version: 16001,
            items: vec![3, 1],
            quests: vec![9],
        };
        assert!(write_list(&dir, &list).unwrap());
        assert!(!write_list(&dir, &list).unwrap());
        let items =
            fs::read_to_string(dir.join("Interface/AddOns").join(ADDON).join(ITEM_FILE)).unwrap();
        assert!(items.contains("    1, 3,\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_caches_of_the_build_are_uploaded() {
        let dir = temp_dir("caches");
        let mut db = b"XFTH".to_vec();
        db.extend(9u32.to_le_bytes());
        db.extend(70707u32.to_le_bytes());
        let mut quest = b"TSQW".to_vec();
        quest.extend(70685u32.to_le_bytes());
        for (path, content) in [
            ("Cache/ADB/enUS/DBCache.bin", db.clone()),
            ("Cache/ADB/deDE/DBCache.bin", db),
            ("Cache/WDB/enUS/questcache.wdb", quest),
        ] {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        let files = cache_files(&dir, 70707);
        let found: Vec<(&str, &str)> = files.iter().map(|f| (f.kind, f.locale.as_str())).collect();
        assert_eq!(found, vec![("dbcache", "deDE"), ("dbcache", "enUS")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_route_sits_next_to_the_snapshot_route() {
        assert_eq!(
            cache_upload_url("https://upload.foreverdb.app/imports/forevercollect"),
            "https://upload.foreverdb.app/imports/client-cache"
        );
    }
}
