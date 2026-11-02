// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! Personal (non-commercial, LICENSE.md section 2) build profile for this fork.
//!
//! Baked at BUILD time by `NEXT_PUBLIC_SCREENPIPE_PERSONAL_BUILD=1`. The TS
//! side reads the same variable (app/providers.tsx), the way `web_base.rs`
//! shares `NEXT_PUBLIC_SCREENPIPE_WEB_URL`, so a single env var configures the
//! whole app without a cargo feature or a change to the build scripts.
//!
//! It changes defaults only:
//! * telemetry off for the process and its children (Sentry, PostHog);
//! * a NEW settings store starts with analytics and remote support logs off,
//!   and with ChatGPT (Codex OAuth) plus Ollama presets in place of the
//!   screenpipe-cloud preset.
//!
//! It never touches sign-in, plan, entitlement, history or pipe checks, and it
//! never rewrites an existing settings store. Unset = upstream behavior.
//! See PERSONAL.md.

use crate::store::{AIPreset, AIProviderType, SettingsStore};

const BUILD_FLAG: Option<&str> = option_env!("NEXT_PUBLIC_SCREENPIPE_PERSONAL_BUILD");

pub fn enabled() -> bool {
    matches!(
        BUILD_FLAG.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Call at the top of `main()`, before anything reads the telemetry kill
/// switch. Child processes (engine, Pi) inherit it.
pub fn apply_process_env() {
    if enabled() {
        set_if_unset("SCREENPIPE_DISABLE_TELEMETRY", "1");
    }
}

/// Apply personal defaults to a settings store this launch just created.
/// Returns true when the store changed and must be saved.
pub fn apply_new_profile_defaults(store: &mut SettingsStore, is_new_store: bool) -> bool {
    apply_defaults_if(store, is_new_store && enabled())
}

fn apply_defaults_if(store: &mut SettingsStore, apply: bool) -> bool {
    if !apply {
        return false;
    }
    store.recording.analytics_enabled = false;
    store.remote_log_collection_enabled = false;
    // Keep upstream's default system prompt; only the provider changes.
    let prompt = store
        .ai_presets
        .iter()
        .find(|p| p.default_preset)
        .map(|p| p.prompt.clone())
        .unwrap_or_default();
    store.ai_presets = vec![
        // Same shape the presets editor creates for ChatGPT. Credentials come
        // from the user's own OAuth login (chatgpt_oauth.rs), never from here.
        AIPreset {
            id: "chatgpt".to_string(),
            prompt: prompt.clone(),
            provider: AIProviderType::OpenAIChatGPT,
            url: "https://api.openai.com/v1".to_string(),
            model: "gpt-5.6-terra".to_string(),
            default_preset: true,
            max_context_chars: 128000,
            ..AIPreset::default()
        },
        // Model left empty, as in the presets editor: the user picks one of
        // their locally pulled Ollama models.
        AIPreset {
            id: "ollama".to_string(),
            prompt,
            provider: AIProviderType::NativeOllama,
            url: "http://localhost:11434/v1".to_string(),
            model: String::new(),
            default_preset: false,
            max_context_chars: 128000,
            ..AIPreset::default()
        },
    ];
    true
}

// Same contract as dev_isolation's private helper: an explicit value wins.
fn set_if_unset(key: &str, value: &str) {
    if std::env::var_os(key).is_some_and(|v| !v.is_empty()) {
        return;
    }
    std::env::set_var(key, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(preset: &AIPreset) -> String {
        serde_json::to_value(&preset.provider)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn new_profile_gets_personal_defaults() {
        let mut store = SettingsStore::default();
        let upstream_prompt = store.ai_presets[0].prompt.clone();
        assert!(apply_defaults_if(&mut store, true));

        assert!(!store.recording.analytics_enabled);
        assert!(!store.remote_log_collection_enabled);
        let providers: Vec<String> = store.ai_presets.iter().map(provider).collect();
        assert_eq!(providers, ["openai-chatgpt", "native-ollama"]);
        assert!(store.ai_presets[0].default_preset);
        assert!(!store.ai_presets[1].default_preset);
        assert_eq!(store.ai_presets[0].prompt, upstream_prompt);
        assert!(store.ai_presets.iter().all(|p| p.api_key.is_none()));
    }

    #[test]
    fn existing_profile_is_never_rewritten() {
        let mut store = SettingsStore::default();
        let before = serde_json::to_value(&store).unwrap();
        assert!(!apply_new_profile_defaults(&mut store, false));
        assert_eq!(serde_json::to_value(&store).unwrap(), before);
    }

    #[test]
    fn unflagged_build_changes_nothing() {
        // The test binary is built without the flag unless the developer set
        // it; in that case this assertion is about the flagged build instead.
        let mut store = SettingsStore::default();
        let before = serde_json::to_value(&store).unwrap();
        let changed = apply_new_profile_defaults(&mut store, true);
        assert_eq!(changed, enabled());
        if !enabled() {
            assert_eq!(serde_json::to_value(&store).unwrap(), before);
        }
    }

    #[test]
    fn set_if_unset_never_overrides_an_explicit_value() {
        let key = "SCREENPIPE_PERSONAL_SET_IF_UNSET_TEST";
        std::env::set_var(key, "0");
        set_if_unset(key, "1");
        assert_eq!(std::env::var(key).unwrap(), "0");
        std::env::set_var(key, "");
        set_if_unset(key, "1");
        assert_eq!(std::env::var(key).unwrap(), "1");
    }
}
