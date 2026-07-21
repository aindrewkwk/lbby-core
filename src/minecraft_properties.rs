use std::collections::HashSet;

/// Update Lbby-managed keys while preserving every unrelated user setting.
/// `online-mode` and `server-port` are only defaulted for a new/missing key.
pub fn merge_server_properties(
    existing: Option<&str>,
    max_players: u32,
    motd: &str,
    default_port: u16,
    view_distance: u8,
    simulation_distance: u8,
) -> String {
    let motd = motd.replace(['\r', '\n'], " ");
    let managed = [
        ("max-players", max_players.to_string()),
        ("motd", motd),
        ("view-distance", view_distance.to_string()),
        ("simulation-distance", simulation_distance.to_string()),
    ];
    let mut seen = HashSet::new();
    let mut output = Vec::new();

    if let Some(text) = existing {
        for line in text.lines() {
            let key = line
                .split_once('=')
                .map(|(key, _)| key.trim())
                .unwrap_or("");
            if let Some((_, value)) = managed.iter().find(|(managed_key, _)| *managed_key == key) {
                if seen.insert(key.to_string()) {
                    output.push(format!("{key}={value}"));
                }
            } else {
                if !key.is_empty() {
                    seen.insert(key.to_string());
                }
                output.push(line.to_string());
            }
        }
    }

    if !seen.contains("online-mode") {
        output.push("online-mode=true".to_string());
    }
    if !seen.contains("server-port") {
        output.push(format!("server-port={default_port}"));
    }
    for (key, value) in managed {
        if !seen.contains(key) {
            output.push(format!("{key}={value}"));
        }
    }
    if !seen.contains("spawn-protection") {
        output.push("spawn-protection=0".to_string());
    }

    format!("{}\n", output.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_server_defaults_to_authenticated_mode() {
        let rendered = merge_server_properties(None, 20, "Hello", 25565, 8, 6);
        assert!(rendered.contains("online-mode=true\n"));
        assert!(rendered.contains("server-port=25565\n"));
    }

    #[test]
    fn preserves_user_settings_and_port() {
        let existing = "online-mode=false\nserver-port=25570\ndifficulty=hard\nmax-players=5\n";
        let rendered = merge_server_properties(Some(existing), 30, "New", 25565, 10, 8);
        assert!(rendered.contains("online-mode=false\n"));
        assert!(rendered.contains("server-port=25570\n"));
        assert!(rendered.contains("difficulty=hard\n"));
        assert!(rendered.contains("max-players=30\n"));
    }
}
