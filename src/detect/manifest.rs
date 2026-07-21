//! Per-agent TOML screen manifests (ported from herdr's detection engine).
//!
//! Regions + gates match herdr's rule language so their Claude/Codex TOMLs
//! can be used almost as-is.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use serde::Deserialize;

use super::{DetectedState, ManifestAgent};

pub const DEFAULT_KNOWN_AGENT_IDLE_FALLBACK: &str = "default_known_agent_idle_fallback";

#[derive(Debug, Clone, Copy)]
pub struct DetectionInput<'a> {
    pub screen: &'a str,
    pub osc_title: &'a str,
    pub osc_progress: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDetection {
    pub state: DetectedState,
    pub skip_state_update: bool,
    pub visible_idle: bool,
    pub visible_blocker: bool,
    pub visible_working: bool,
    pub matched_rule: Option<String>,
    pub fallback_reason: Option<&'static str>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct AgentManifest {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    version: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    min_engine_version: Option<u32>,
    #[serde(default, rename = "updated_at")]
    #[allow(dead_code)]
    updated_at: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    aliases: Vec<String>,
    #[serde(default)]
    rules: Vec<ManifestRule>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestRule {
    id: String,
    state: Option<ManifestState>,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_region")]
    region: String,
    #[serde(default)]
    visible_idle: bool,
    #[serde(default)]
    visible_blocker: bool,
    #[serde(default)]
    visible_working: bool,
    #[serde(default)]
    skip_state_update: bool,
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestGate {
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ManifestState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl From<ManifestState> for DetectedState {
    fn from(value: ManifestState) -> Self {
        match value {
            ManifestState::Idle => DetectedState::Idle,
            ManifestState::Working => DetectedState::Working,
            ManifestState::Blocked => DetectedState::Blocked,
            ManifestState::Unknown => DetectedState::Unknown,
        }
    }
}

fn default_region() -> String {
    "whole_recent".to_string()
}

#[derive(Debug, Clone)]
struct CompiledGate {
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not_gate: Vec<CompiledGate>,
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    gate: CompiledGate,
}

#[derive(Debug, Clone)]
struct LoadedManifest {
    manifest: AgentManifest,
    compiled_rules: Vec<CompiledRule>,
}

const BUNDLED: &[(&str, &str)] = &[
    ("claude", include_str!("manifests/claude.toml")),
    ("codex", include_str!("manifests/codex.toml")),
    ("grok", include_str!("manifests/grok.toml")),
    ("cursor", include_str!("manifests/cursor.toml")),
    ("gemini", include_str!("manifests/gemini.toml")),
    ("opencode", include_str!("manifests/opencode.toml")),
    ("amp", include_str!("manifests/amp.toml")),
];

type ManifestCache = Vec<(ManifestAgent, Option<LoadedManifest>)>;

fn cache() -> &'static Mutex<ManifestCache> {
    static CACHE: OnceLock<Mutex<ManifestCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut items = ManifestCache::new();
        for agent in ManifestAgent::all() {
            items.push((*agent, load_manifest_uncached(*agent)));
        }
        Mutex::new(items)
    })
}

fn load_manifest(agent: ManifestAgent) -> Option<LoadedManifest> {
    let guard = cache().lock().unwrap_or_else(|p| p.into_inner());
    guard
        .iter()
        .find(|(a, _)| *a == agent)
        .and_then(|(_, m)| m.clone())
}

fn load_manifest_uncached(agent: ManifestAgent) -> Option<LoadedManifest> {
    // Local override wins: ~/.config/vmux/agent-detection/<agent>.toml
    if let Some(path) = override_path(agent) {
        if path.is_file() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(loaded) = parse_manifest(&text) {
                    return Some(loaded);
                }
            }
        }
    }
    let text = BUNDLED
        .iter()
        .find(|(id, _)| *id == agent.label())
        .map(|(_, t)| *t)?;
    parse_manifest(text).ok()
}

fn override_path(agent: ManifestAgent) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".config")
            .join("vmux")
            .join("agent-detection")
            .join(format!("{}.toml", agent.label())),
    )
}

fn parse_manifest(text: &str) -> Result<LoadedManifest, String> {
    let manifest: AgentManifest =
        toml::from_str(text).map_err(|e| format!("parse manifest: {e}"))?;
    let compiled_rules = compile_manifest(&manifest)?;
    Ok(LoadedManifest {
        manifest,
        compiled_rules,
    })
}

fn compile_manifest(manifest: &AgentManifest) -> Result<Vec<CompiledRule>, String> {
    manifest
        .rules
        .iter()
        .map(|rule| {
            compile_gate(&gate_from_rule(rule))
                .map(|gate| CompiledRule { gate })
                .map_err(|e| format!("rule {}: {e}", rule.id))
        })
        .collect()
}

fn gate_from_rule(rule: &ManifestRule) -> ManifestGate {
    ManifestGate {
        all: rule.all.clone(),
        any: rule.any.clone(),
        not_gate: rule.not_gate.clone(),
        contains: rule.contains.clone(),
        regex: rule.regex.clone(),
        line_regex: rule.line_regex.clone(),
    }
}

fn compile_gate(gate: &ManifestGate) -> Result<CompiledGate, String> {
    Ok(CompiledGate {
        all: gate
            .all
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        any: gate
            .any
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        not_gate: gate
            .not_gate
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        contains: gate.contains.iter().map(|n| n.to_lowercase()).collect(),
        regex: gate
            .regex
            .iter()
            .map(|p| Regex::new(p).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?,
        line_regex: gate
            .line_regex
            .iter()
            .map(|p| Regex::new(p).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?,
    })
}

pub fn detect_with_osc(agent: ManifestAgent, input: DetectionInput<'_>) -> ManifestDetection {
    let Some(loaded) = load_manifest(agent) else {
        return ManifestDetection {
            state: DetectedState::Idle,
            skip_state_update: false,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            matched_rule: None,
            fallback_reason: Some(DEFAULT_KNOWN_AGENT_IDLE_FALLBACK),
        };
    };

    let mut matched: Option<&ManifestRule> = None;
    for (rule, compiled) in loaded.manifest.rules.iter().zip(&loaded.compiled_rules) {
        let region_text = region(input, &rule.region);
        if !compiled_rule_matches(compiled, region_text) {
            continue;
        }
        match matched {
            Some(prev) if prev.priority >= rule.priority => {}
            _ => matched = Some(rule),
        }
    }

    let Some(rule) = matched else {
        return ManifestDetection {
            state: DetectedState::Idle,
            skip_state_update: false,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            matched_rule: None,
            fallback_reason: Some(DEFAULT_KNOWN_AGENT_IDLE_FALLBACK),
        };
    };

    let state = rule
        .state
        .map(DetectedState::from)
        .unwrap_or(DetectedState::Unknown);
    ManifestDetection {
        state,
        skip_state_update: rule.skip_state_update,
        visible_idle: rule.visible_idle && state == DetectedState::Idle,
        visible_blocker: rule.visible_blocker && state == DetectedState::Blocked,
        visible_working: rule.visible_working && state == DetectedState::Working,
        matched_rule: Some(rule.id.clone()),
        fallback_reason: None,
    }
}

fn compiled_rule_matches(rule: &CompiledRule, text: &str) -> bool {
    let lower = text.to_lowercase();
    compiled_gate_matches(&rule.gate, text, &lower)
}

fn compiled_gate_matches(gate: &CompiledGate, text: &str, lower_text: &str) -> bool {
    if !gate.contains.iter().all(|n| lower_text.contains(n)) {
        return false;
    }
    if !gate.regex.iter().all(|r| r.is_match(text)) {
        return false;
    }
    if !gate
        .line_regex
        .iter()
        .all(|r| text.lines().any(|line| r.is_match(line)))
    {
        return false;
    }
    if !gate
        .all
        .iter()
        .all(|n| compiled_gate_matches(n, text, lower_text))
    {
        return false;
    }
    if !gate.any.is_empty()
        && !gate
            .any
            .iter()
            .any(|n| compiled_gate_matches(n, text, lower_text))
    {
        return false;
    }
    if gate
        .not_gate
        .iter()
        .any(|n| compiled_gate_matches(n, text, lower_text))
    {
        return false;
    }
    true
}

// ── regions (herdr-compatible) ──────────────────────────────────────────

fn region<'a>(input: DetectionInput<'a>, spec: &str) -> &'a str {
    let trimmed = spec.trim();
    match trimmed {
        "osc_title" => return input.osc_title,
        "osc_progress" => return input.osc_progress,
        _ => {}
    }
    let content = input.screen;
    match trimmed {
        "whole_recent" => content,
        "after_last_prompt_marker" => after_last_prompt_marker(content),
        "before_current_prompt_marker" => before_current_prompt_marker(content),
        "whole_recent_without_current_prompt_marker" => {
            whole_recent_without_current_prompt_marker(content)
        }
        "current_prompt_block_marker" => current_prompt_block_marker(content).unwrap_or(""),
        "after_current_prompt_block_marker" => {
            after_current_prompt_block_marker(content).unwrap_or("")
        }
        "prompt_box_body" => prompt_box_body(content).unwrap_or(""),
        "above_prompt_box" => above_prompt_box(content),
        "last_non_empty_above_prompt_box" => last_non_empty_line(above_prompt_box(content)),
        "after_last_horizontal_rule" => after_last_horizontal_rule(content),
        _ => {
            if let Some(count) = region_count(trimmed, "bottom_lines") {
                return bottom_lines(content, count);
            }
            if let Some(count) = region_count(trimmed, "bottom_non_empty_lines") {
                return bottom_non_empty_lines(content, count);
            }
            if let Some(count) = top_region_count(trimmed) {
                return top_non_empty_lines(content, count);
            }
            ""
        }
    }
}

fn region_count(spec: &str, name: &str) -> Option<usize> {
    spec.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|count| count.parse().ok())
}

fn top_region_count(spec: &str) -> Option<usize> {
    let count = spec
        .strip_prefix("top_non_empty_lines")?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    if count.starts_with('0') || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    count.parse().ok()
}

fn bottom_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(count);
    slice_from_line_index(content, &lines, start)
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(start_index) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    slice_from_line_index(content, &lines, start_index)
}

fn top_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(end_index) = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    let byte_offset = line_start_offset(content, &lines, end_index + 1);
    &content[..byte_offset]
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = lines.iter().rposition(|line| codex_prompt_line(line)) else {
        return content;
    };
    slice_from_line_index(content, &lines, index + 1)
}

fn before_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = current_codex_prompt_index(&lines) else {
        return content;
    };
    let byte_offset = lines[..index]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>();
    &content[..byte_offset.min(content.len())]
}

fn whole_recent_without_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    if current_codex_prompt_index(&lines).is_some() {
        ""
    } else {
        content
    }
}

fn current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = current_codex_prompt_index(&lines)?;
    lines[..prompt_index]
        .iter()
        .rev()
        .find(|line| codex_block_marker_line(line))
        .copied()
}

fn after_current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = current_codex_prompt_index(&lines)?;
    let block_index = lines[..prompt_index]
        .iter()
        .rposition(|line| codex_block_marker_line(line))?;
    Some(slice_from_line_index(content, &lines, block_index))
}

fn current_codex_prompt_index(lines: &[&str]) -> Option<usize> {
    let prompt_index = lines.iter().rposition(|line| codex_prompt_line(line))?;
    if lines[prompt_index + 1..]
        .iter()
        .any(|line| codex_block_marker_line(line))
    {
        return None;
    }
    Some(prompt_index)
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker_line(line: &str) -> bool {
    line.starts_with('•') || line.starts_with('■') || line.starts_with('✗') || line.starts_with('✓')
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let top = prompt_box_top_border_index(&lines)?;
    let start = line_start_offset(content, &lines, top + 1);
    let end_index = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map(|relative| top + 1 + relative)
        .unwrap_or(lines.len());
    let end = line_start_offset(content, &lines, end_index);
    Some(&content[start.min(content.len())..end.min(content.len())])
}

fn above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(top) = prompt_box_top_border_index(&lines) else {
        return content;
    };
    let end = line_start_offset(content, &lines, top);
    &content[..end.min(content.len())]
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut last_rule_end = 0usize;
    let mut offset = 0usize;
    for line in content.lines() {
        let next_offset = offset + line.len() + 1;
        if is_horizontal_rule(line) {
            last_rule_end = next_offset.min(content.len());
        }
        offset = next_offset;
    }
    &content[last_rule_end..]
}

fn last_non_empty_line(content: &str) -> &str {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

fn prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut border_count = 0;
    for index in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[index]) {
            border_count += 1;
            if border_count == 2 {
                return Some(index);
            }
        }
    }
    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let rule_chars = trimmed.chars().take_while(|&ch| ch == '─').count();
    if rule_chars == 0 {
        return false;
    }
    let rule_bytes = trimmed
        .char_indices()
        .nth(rule_chars)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    let suffix = trimmed[rule_bytes..].trim_start();
    suffix.is_empty() || rule_chars >= 3
}

fn slice_from_line_index<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    let byte_offset = line_start_offset(content, lines, index);
    &content[byte_offset.min(content.len())..]
}

fn line_start_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}
