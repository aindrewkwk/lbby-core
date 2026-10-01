//! Authoritative properties edits: latest disk state, preserved unrelated text,
//! serialized in-process writes, and same-directory atomic publication.
use std::collections::HashMap;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::Mutex;

static PROPERTIES_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Existing files are authoritative. Defaults are only for a genuinely new file.
pub fn merge_server_properties(
    existing: Option<&str>,
    max_players: u32,
    motd: &str,
    default_port: u16,
    view_distance: u8,
    simulation_distance: u8,
    online_mode: Option<bool>,
) -> String {
    if let Some(existing) = existing {
        return existing.to_owned();
    }
    format!("online-mode={}\nserver-port={}\nmax-players={}\nmotd={}\nview-distance={}\nsimulation-distance={}\nspawn-protection=0\n",
        online_mode.unwrap_or(true), default_port, max_players,
        motd.replace(['\r', '\n'], " "), view_distance, simulation_distance)
}

#[derive(Debug)]
struct Entry {
    raw: String,
    key: Option<String>,
    value: String,
}

/// Small extension of the existing line-preserving parser. Values remain in
/// properties-file encoding (backslash escapes are not decoded by this API).
struct Document {
    entries: Vec<Entry>,
    newline: &'static str,
}
impl Document {
    fn parse(text: &str) -> Result<Self, String> {
        validate_unicode_escapes(text)?;
        let mut entries = Vec::new();
        let mut raw = String::new();
        for line in text.split_inclusive('\n') {
            raw.push_str(line);
            let body = line.trim_end_matches(['\r', '\n']);
            let continuation = body.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1;
            // Comment backslashes do not continue a Java properties record.
            let comment = raw.trim_start().starts_with(['#', '!']);
            if continuation && !comment {
                continue;
            }
            entries.push(Self::entry(std::mem::take(&mut raw)));
        }
        if !raw.is_empty() {
            entries.push(Self::entry(raw));
        }
        Ok(Self {
            entries,
            newline: if text.contains("\r\n") { "\r\n" } else { "\n" },
        })
    }
    fn entry(raw: String) -> Entry {
        let logical = raw.trim_end_matches(['\r', '\n']);
        let text = logical.trim_start();
        if text.is_empty() || text.starts_with(['#', '!']) {
            return Entry {
                raw,
                key: None,
                value: String::new(),
            };
        }
        let mut escaped = false;
        let mut split = text.len();
        for (i, ch) in text.char_indices() {
            if !escaped && (ch == '=' || ch == ':' || matches!(ch, ' ' | '\t' | '\x0c')) {
                split = i;
                break;
            }
            escaped = !escaped && ch == '\\';
        }
        let key = text[..split].to_owned();
        let tail = &text[split..];
        let value = if tail.starts_with(['=', ':']) {
            &tail[1..]
        } else {
            let tail = tail.trim_start_matches([' ', '\t', '\x0c']);
            tail.strip_prefix(['=', ':']).unwrap_or(tail)
        }
        .to_owned();
        Entry {
            raw,
            key: Some(key),
            value,
        }
    }
    fn to_map(&self) -> HashMap<String, String> {
        // Last occurrence wins, matching Java and the previous core reader.
        self.entries
            .iter()
            .filter_map(|e| e.key.as_ref().map(|k| (k.clone(), e.value.clone())))
            .collect()
    }
    fn merge(&mut self, props: &HashMap<String, String>) -> Result<(), String> {
        let mut sorted: Vec<_> = props.iter().collect();
        sorted.sort_by_key(|(k, _)| *k);
        for (key, value) in sorted {
            if Self::entry(format!("{}=", key)).key.as_ref() != Some(key) {
                return Err(
                    "Cannot parse server.properties update: unescaped key separator".into(),
                );
            }
            if key.is_empty() || key.contains(['\r', '\n', '\0']) || value.contains('\0') {
                return Err(
                    "Cannot parse server.properties update: invalid key or NUL value".into(),
                );
            }
            // Keep untouched records byte-for-byte, including continuations and
            // spacing. Missing incoming keys never imply deletion.
            let mut found = false;
            for entry in &mut self.entries {
                if entry.key.as_ref() == Some(key) {
                    found = true;
                    if entry.value != *value {
                        entry.raw = format!("{}={}{}", key, escape_newlines(value), self.newline);
                        entry.value = value.clone();
                    }
                }
            }
            if !found {
                if let Some(last) = self.entries.last_mut() {
                    if !last.raw.ends_with('\n') {
                        last.raw.push_str(self.newline);
                    }
                }
                self.entries.push(Entry {
                    raw: format!("{}={}{}", key, escape_newlines(value), self.newline),
                    key: Some(key.clone()),
                    value: value.clone(),
                });
            }
        }
        Ok(())
    }
    fn text(&self) -> String {
        self.entries.iter().map(|e| e.raw.as_str()).collect()
    }
}
fn escape_newlines(value: &str) -> String {
    value.replace('\r', "\\r").replace('\n', "\\n")
}
fn validate_unicode_escapes(text: &str) -> Result<(), String> {
    // Java rejects malformed Unicode escapes. Preserve all other legacy/raw
    // lines, comments and escape sequences rather than enforcing a whitelist.
    for line in text
        .lines()
        .filter(|line| !line.trim_start().starts_with(['#', '!']))
    {
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' && chars.next() == Some('u') {
                for _ in 0..4 {
                    if !chars.next().is_some_and(|c| c.is_ascii_hexdigit()) {
                        return Err(
                            "Cannot parse server.properties: malformed Unicode escape".into()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Only NotFound means absence. UTF-8, permissions and other IO errors propagate.
pub fn read_properties_text(path: &Path) -> Result<Option<String>, String> {
    #[cfg(test)]
    if let Some(kind) = FORCE_READ_ERROR.with(|error| error.get()) {
        return Err(format!(
            "Cannot read server.properties ({:?}): injected read failure",
            kind
        ));
    }
    match std::fs::read_to_string(path) {
        Ok(text) => {
            Document::parse(&text)?;
            Ok(Some(text))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == ErrorKind::InvalidData => {
            Err("Cannot parse server.properties: invalid UTF-8".into())
        }
        Err(e) => Err(format!(
            "Cannot read server.properties ({:?}): {}",
            e.kind(),
            e
        )),
    }
}
pub fn read_properties_map(path: &Path) -> Result<HashMap<String, String>, String> {
    Document::parse(read_properties_text(path)?.as_deref().unwrap_or("")).map(|doc| doc.to_map())
}

/// Partial patch, under the same lock used by installers and config sync.
pub fn merge_properties_file(path: &Path, props: &HashMap<String, String>) -> Result<(), String> {
    let _guard = PROPERTIES_WRITE_LOCK
        .lock()
        .map_err(|_| "Properties write lock poisoned")?;
    merge_locked(path, props, false).map(|_| ())
}
pub fn patch_existing_properties(
    path: &Path,
    props: &HashMap<String, String>,
) -> Result<bool, String> {
    let _guard = PROPERTIES_WRITE_LOCK
        .lock()
        .map_err(|_| "Properties write lock poisoned")?;
    merge_locked(path, props, true)
}
fn merge_locked(
    path: &Path,
    props: &HashMap<String, String>,
    existing_only: bool,
) -> Result<bool, String> {
    let original = read_properties_text(path)?;
    if original.is_none() && existing_only {
        return Ok(false);
    }
    let mut doc = Document::parse(original.as_deref().unwrap_or(""))?;
    doc.merge(props)?;
    let output = doc.text();
    Document::parse(&output)?;
    if original.as_deref() != Some(&output) {
        atomic_properties_text(path, &output, false)?;
    }
    Ok(true)
}

/// Missing-file creation publishes without replacing a file that appears in
/// the meantime. Existing files are validated/read but never rewritten.
pub fn ensure_properties_file(path: &Path, template: &str) -> Result<(), String> {
    let _guard = PROPERTIES_WRITE_LOCK
        .lock()
        .map_err(|_| "Properties write lock poisoned")?;
    if read_properties_text(path)?.is_some() {
        return Ok(());
    }
    Document::parse(template)?;
    atomic_properties_text(path, template, true)?;
    read_properties_text(path)?;
    Ok(())
}

/// Same-directory temp -> write_all -> sync_all -> rename, matching existing
/// persistence conventions. Creation uses atomic hard-link publication so it
/// cannot overwrite a concurrently created authoritative file.
pub fn atomic_properties_text(path: &Path, text: &str, create_only: bool) -> Result<(), String> {
    let tmp = path.with_file_name(format!(".lbby-properties-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err("Cannot write server.properties: symbolic link target".into())
            }
            Ok(m) => Some(m),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(format!("Cannot read server.properties metadata: {}", e)),
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| format!("Cannot write server.properties temp file: {}", e))?;
        if let Some(m) = metadata {
            file.set_permissions(m.permissions())
                .map_err(|e| format!("Cannot write server.properties permissions: {}", e))?;
        }
        file.write_all(text.as_bytes())
            .map_err(|e| format!("Cannot write server.properties temp file: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Cannot sync server.properties temp file: {}", e))?;
        drop(file);
        #[cfg(test)]
        if FORCE_REPLACE_ERROR.with(|error| error.get()) {
            return Err("Cannot atomically replace server.properties: injected failure".into());
        }
        if create_only {
            match std::fs::hard_link(&tmp, path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("Cannot atomically create server.properties: {}", e)),
            }
        } else {
            std::fs::rename(&tmp, path)
                .map_err(|e| format!("Cannot atomically replace server.properties: {}", e))?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

#[cfg(test)]
std::thread_local! {
    static FORCE_READ_ERROR: std::cell::Cell<Option<ErrorKind>> = const { std::cell::Cell::new(None) };
    static FORCE_REPLACE_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.properties");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }
    fn patch(path: &Path, key: &str, value: &str) {
        merge_properties_file(path, &HashMap::from([(key.into(), value.into())])).unwrap();
    }
    struct ReadErrorGuard;
    impl Drop for ReadErrorGuard {
        fn drop(&mut self) {
            FORCE_READ_ERROR.with(|f| f.set(None));
        }
    }
    struct ReplaceErrorGuard;
    impl Drop for ReplaceErrorGuard {
        fn drop(&mut self) {
            FORCE_REPLACE_ERROR.with(|f| f.set(false));
        }
    }

    #[test]
    fn partial_payload_preserves_unrelated_keys() {
        let (_dir, path) = fixture("motd=A\nserver-port=25565\ncustom-key=foo\n");
        patch(&path, "motd", "B");
        let props = read_properties_map(&path).unwrap();
        assert_eq!(props["motd"], "B");
        assert_eq!(props["server-port"], "25565");
        assert_eq!(props["custom-key"], "foo");
    }
    #[test]
    fn unknown_key_survives_known_property_save() {
        let (_dir, path) = fixture("unknown-plugin-key=abc\n");
        patch(&path, "motd", "B");
        assert_eq!(
            read_properties_map(&path).unwrap()["unknown-plugin-key"],
            "abc"
        );
    }
    #[test]
    fn stale_payload_preserves_new_disk_keys() {
        let (_dir, path) = fixture("motd=A\nserver-port=25565\n");
        let mut frontend = read_properties_map(&path).unwrap();
        std::fs::write(&path, "motd=A\nserver-port=25565\nadded-after-load=keep\n").unwrap();
        frontend.insert("motd".into(), "B".into());
        merge_properties_file(&path, &frontend).unwrap();
        assert_eq!(
            read_properties_map(&path).unwrap()["added-after-load"],
            "keep"
        );
    }
    #[test]
    fn missing_file_gets_only_seven_template_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.properties");
        let template = merge_server_properties(None, 20, "Hello", 25565, 8, 6, None);
        ensure_properties_file(&path, &template).unwrap();
        let props = read_properties_map(&path).unwrap();
        assert_eq!(props.len(), 7);
        assert_eq!(props["online-mode"], "true");
    }
    #[test]
    fn existing_file_is_authoritative_to_installer() {
        let original = "# Imported settings\r\nmotd=Custom\r\nmax-players=99\r\nunknown=keep\r\n";
        let (_dir, path) = fixture(original);
        let template = merge_server_properties(None, 20, "Default", 25565, 8, 6, Some(true));
        ensure_properties_file(&path, &template).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            merge_server_properties(Some(original), 20, "Default", 25565, 8, 6, Some(true)),
            original
        );
    }
    #[test]
    fn permission_read_failure_never_overwrites_existing_file() {
        let (_dir, path) = fixture("motd=Original\n");
        FORCE_READ_ERROR.with(|f| f.set(Some(ErrorKind::PermissionDenied)));
        let guard = ReadErrorGuard;
        assert!(ensure_properties_file(&path, "motd=Default\n")
            .unwrap_err()
            .contains("PermissionDenied"));
        assert!(
            merge_properties_file(&path, &HashMap::from([("motd".into(), "B".into())])).is_err()
        );
        drop(guard);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "motd=Original\n");
    }
    #[test]
    fn real_io_failure_directory_is_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.properties");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), "original").unwrap();
        assert!(ensure_properties_file(&path, "motd=Default\n").is_err());
        assert!(
            merge_properties_file(&path, &HashMap::from([("motd".into(), "B".into())])).is_err()
        );
        assert_eq!(
            std::fs::read_to_string(path.join("keep")).unwrap(),
            "original"
        );
    }
    #[test]
    fn invalid_utf8_never_overwritten() {
        let (_dir, path) = fixture("");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(ensure_properties_file(&path, "motd=Default\n")
            .unwrap_err()
            .contains("invalid UTF-8"));
        assert_eq!(std::fs::read(&path).unwrap(), [0xff, 0xfe]);
    }
    #[test]
    fn malformed_unicode_never_overwritten() {
        let original = r"motd=\uZZZZ";
        let (_dir, path) = fixture(original);
        assert!(ensure_properties_file(&path, "motd=Default\n")
            .unwrap_err()
            .contains("Cannot parse"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
    #[test]
    fn comments_and_crlf_survive_edit() {
        let (_dir, path) =
            fixture("# hello\r\n! keep too\r\n\r\nmotd=A\r\ncustom : value=with:separators\r\n");
        patch(&path, "motd", "B");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "# hello\r\n! keep too\r\n\r\nmotd=B\r\ncustom : value=with:separators\r\n"
        );
    }
    #[test]
    fn empty_value_is_valid() {
        let (_dir, path) = fixture("motd=A\ncustom=keep\n");
        patch(&path, "motd", "");
        assert_eq!(read_properties_map(&path).unwrap()["motd"], "");
        assert!(std::fs::read_to_string(path).unwrap().contains("motd=\n"));
    }
    #[test]
    fn imported_full_fixture_does_not_shrink() {
        let text = (0..40)
            .map(|i| format!("arbitrary-property-{}=value-{}\n", i, i))
            .collect::<String>();
        let (_dir, path) = fixture(&text);
        ensure_properties_file(
            &path,
            &merge_server_properties(None, 20, "Default", 25565, 8, 6, None),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        patch(&path, "motd", "B");
        let props = read_properties_map(&path).unwrap();
        assert_eq!(props.len(), 41);
        for i in 0..40 {
            assert_eq!(
                props[&format!("arbitrary-property-{}", i)],
                format!("value-{}", i)
            );
        }
    }
    #[test]
    fn duplicate_keys_are_updated_without_appending() {
        let (_dir, path) = fixture("motd=First\nmotd=Last\ncustom=keep\n");
        assert_eq!(read_properties_map(&path).unwrap()["motd"], "Last");
        patch(&path, "motd", "New");
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.matches("motd=").count(), 2);
        assert_eq!(text.matches("motd=New").count(), 2);
    }
    #[test]
    fn encoded_values_unicode_spaces_and_equals_round_trip() {
        let original = "# note\nempty=\nvalue= a=b:c  \nunicode=Tiếng Việt\npath=C:\\Games\\mods\ncontinued=hello\\\n  world\n";
        let (_dir, path) = fixture(original);
        let props = read_properties_map(&path).unwrap();
        merge_properties_file(&path, &props).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        patch(&path, "value", " x=y:z  ");
        assert_eq!(read_properties_map(&path).unwrap()["value"], " x=y:z  ");
    }
    #[test]
    fn actual_newlines_cannot_inject_another_property() {
        let (_dir, path) = fixture("motd=A\n");
        patch(&path, "motd", "Hello\nserver-port=123");
        let props = read_properties_map(&path).unwrap();
        assert_eq!(props.len(), 1);
        assert_eq!(props["motd"], r"Hello\nserver-port=123");
    }
    #[test]
    fn atomic_replace_failure_preserves_original_and_cleans_temp() {
        let (dir, path) = fixture("motd=Original\n");
        FORCE_REPLACE_ERROR.with(|f| f.set(true));
        let guard = ReplaceErrorGuard;
        assert!(
            merge_properties_file(&path, &HashMap::from([("motd".into(), "B".into())]))
                .unwrap_err()
                .contains("atomically replace")
        );
        drop(guard);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "motd=Original\n");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn create_only_publication_cannot_replace_existing_file() {
        let (_dir, path) = fixture("motd=Original\n");
        atomic_properties_text(&path, "motd=Default\n", true).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "motd=Original\n");
    }
    #[test]
    fn concurrent_partial_writes_keep_all_keys() {
        let (_dir, path) = fixture("motd=A\n");
        let threads = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || patch(&path, &format!("writer-{}", i), "keep"))
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(read_properties_map(&path).unwrap().len(), 9);
    }
    #[test]
    fn missing_sync_is_a_noop_but_read_errors_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.properties");
        assert!(!patch_existing_properties(&path, &HashMap::new()).unwrap());
        assert!(!path.exists());
    }
}
