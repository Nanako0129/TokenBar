use once_cell::sync::Lazy;
use std::collections::HashMap;

static MODEL_ALIASES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("big-pickle", "glm-4.7");
    m.insert("big pickle", "glm-4.7");
    m.insert("bigpickle", "glm-4.7");
    m.insert("k2p5", "kimi-k2-thinking");
    m.insert("k2-p5", "kimi-k2-thinking");
    m.insert("k2p6", "kimi-k2.6");
    m.insert("k2-p6", "kimi-k2.6");
    m.insert("kimi-k2p6", "kimi-k2.6");
    m.insert("kimi-k2.5-thinking", "kimi-k2-thinking");
    m.insert("kimi-for-coding", "kimi-k2.5");

    m.insert("model_placeholder_m26", "claude-opus-4-6");
    m.insert("model_placeholder_m35", "claude-sonnet-4-6");
    m.insert("model_placeholder_m36", "gemini-3.1-pro");
    m.insert("model_placeholder_m37", "gemini-3.1-pro");
    // Antigravity uses opaque placeholder IDs in IDE metadata and shorter
    // responseModel aliases in CLI conversation protobufs. Keep these as
    // machine-ID aliases rather than display labels because labels may be
    // renamed or localized.
    //
    // M133/`gemini-3-flash-b`, `gemini-3-flash-a`, and M187/raw
    // `gemini-3.5-flash-low` are source-verified exceptions to the obvious
    // mapping: M133 and both response aliases are the High tier; the raw
    // `gemini-3.5-flash-low` wire value is the Medium tier; M187 is the true
    // Low tier with its distinct machine id.
    m.insert("model_placeholder_m16", "gemini-3.1-pro");
    m.insert("model_placeholder_m18", "gemini-3-flash-preview");
    m.insert("model_placeholder_m84", "gemini-3-flash-preview");
    m.insert("model_placeholder_m132", "gemini-3.5-flash-high");
    m.insert("model_placeholder_m133", "gemini-3.5-flash-high");
    m.insert("model_placeholder_m187", "gemini-3.5-flash-extra-low");
    m.insert("model_placeholder_m20", "gemini-3.5-flash-medium");
    m.insert("gemini-pro-default", "gemini-3.1-pro");
    m.insert("gemini-pro-agent", "gemini-3.1-pro");
    m.insert("gemini-3-flash-agent", "gemini-3.5-flash-high");
    m.insert("gemini-3-flash-b", "gemini-3.5-flash-high");
    m.insert("gemini-3.5-flash-low", "gemini-3.5-flash-medium");
    m.insert("model_placeholder_m47", "gemini-3-flash-preview");
    m.insert("model_openai_gpt_oss_120b_medium", "gpt-oss-120b-medium");
    m.insert("claude-opus-4-6-thinking", "claude-opus-4-6");
    m.insert("claude-sonnet-4-6-thinking", "claude-sonnet-4-6");
    m.insert("claude-opus-4.6-thinking", "claude-opus-4-6");
    m.insert("claude-sonnet-4.6-thinking", "claude-sonnet-4-6");
    m.insert("claude-opus-4-6", "claude-opus-4-6");
    m.insert("claude-sonnet-4-6", "claude-sonnet-4-6");
    m.insert("claude-haiku-4-6", "claude-haiku-4-6");
    m.insert("claude-opus-4.6", "claude-opus-4-6");
    m.insert("claude-sonnet-4.6", "claude-sonnet-4-6");
    m.insert("claude-haiku-4.6", "claude-haiku-4-6");
    m.insert("anthropic/claude-4-5-opus", "claude-opus-4-5");
    m.insert("anthropic/claude-4-5-sonnet", "claude-sonnet-4-5");
    m.insert("anthropic/claude-4-5-haiku", "claude-haiku-4-5");
    m.insert("anthropic/claude-4-6-opus", "claude-opus-4-6");
    m.insert("anthropic/claude-4-6-sonnet", "claude-sonnet-4-6");
    m.insert("anthropic/claude-4-6-haiku", "claude-haiku-4-6");
    m.insert("gemini-3.1-pro-high", "gemini-3.1-pro");
    m.insert("gemini-3.1-pro-low", "gemini-3.1-pro");
    m.insert("gemini-3-pro-high", "gemini-3-pro");
    m.insert("gemini-3-pro-low", "gemini-3-pro");
    m.insert("gemini-3-flash", "gemini-3-flash-preview");
    m.insert("gemini-3-flash-c", "gemini-3-flash-preview");
    m.insert("gemini-3-flash-a", "gemini-3.5-flash-high");

    // Synthetic model variants (only where resolver needs help)
    m.insert("kimi-k2.5-nvfp4", "kimi-k2.5"); // Quantization variant → base model pricing
    m.insert("kimi-k2-instruct-0905", "kimi-k2.5"); // Specific version → base (avoids reseller)
    m
});

pub fn resolve_alias(model_id: &str) -> Option<&'static str> {
    MODEL_ALIASES.get(model_id.to_lowercase().as_str()).copied()
}

#[cfg(test)]
mod tests {
    use super::resolve_alias;

    #[test]
    fn resolves_antigravity_placeholders() {
        let cases = [
            ("MODEL_PLACEHOLDER_M26", "claude-opus-4-6"),
            ("model_placeholder_m37", "gemini-3.1-pro"),
            ("model_placeholder_m16", "gemini-3.1-pro"),
            ("model_placeholder_m18", "gemini-3-flash-preview"),
            ("MODEL_PLACEHOLDER_M84", "gemini-3-flash-preview"),
            ("model_placeholder_m132", "gemini-3.5-flash-high"),
            ("model_placeholder_m133", "gemini-3.5-flash-high"),
            ("model_placeholder_m187", "gemini-3.5-flash-extra-low"),
            ("model_placeholder_m20", "gemini-3.5-flash-medium"),
            ("gemini-pro-default", "gemini-3.1-pro"),
            ("gemini-pro-agent", "gemini-3.1-pro"),
            ("gemini-3-flash-agent", "gemini-3.5-flash-high"),
            ("gemini-3-flash-b", "gemini-3.5-flash-high"),
            ("gemini-3.5-flash-low", "gemini-3.5-flash-medium"),
            ("MODEL_OPENAI_GPT_OSS_120B_MEDIUM", "gpt-oss-120b-medium"),
            ("gemini-3-flash-c", "gemini-3-flash-preview"),
            ("gemini-3-flash-a", "gemini-3.5-flash-high"),
            ("claude-opus-4.6-thinking", "claude-opus-4-6"),
            ("anthropic/claude-4-5-haiku", "claude-haiku-4-5"),
            ("anthropic/claude-4-6-sonnet", "claude-sonnet-4-6"),
        ];

        for (raw, expected) in cases {
            assert_eq!(resolve_alias(raw), Some(expected), "raw model: {raw}");
        }
    }

    #[test]
    fn resolves_kimi_k2p6_aliases_without_regressing_k2p5() {
        assert_eq!(resolve_alias("k2p6"), Some("kimi-k2.6"));
        assert_eq!(resolve_alias("k2-p6"), Some("kimi-k2.6"));
        assert_eq!(resolve_alias("kimi-k2p6"), Some("kimi-k2.6"));
        assert_eq!(resolve_alias("KIMI-K2P6"), Some("kimi-k2.6"));

        assert_eq!(resolve_alias("k2p5"), Some("kimi-k2-thinking"));
        assert_eq!(resolve_alias("k2-p5"), Some("kimi-k2-thinking"));
    }

    #[test]
    fn antigravity_low_and_medium_aliases_remain_distinct() {
        let low = resolve_alias("model_placeholder_m187").unwrap();
        let medium = resolve_alias("model_placeholder_m20").unwrap();
        let cli_medium = resolve_alias("gemini-3.5-flash-low").unwrap();

        assert_eq!(low, "gemini-3.5-flash-extra-low");
        assert_eq!(medium, "gemini-3.5-flash-medium");
        assert_ne!(low, medium);
        assert_eq!(cli_medium, medium);
    }
}
