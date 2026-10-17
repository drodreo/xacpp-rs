//! Multimodal content base types.

use serde::{Deserialize, Serialize};

/// serde default for legacy wire data missing `part_type`.
fn default_part_type() -> String {
    "default".to_string()
}

/// File reference (protocol-level simplified version).
///
/// Corresponds to x-agent's `FileRef`, keeping only fields required for protocol transmission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileRef {
    /// Remote URL.
    #[serde(default)]
    pub remote_url: String,
    /// Local file path.
    #[serde(default)]
    pub local_uri: String,
    /// Remote URL expiration time (UTC ISO 8601).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_expires_at: Option<String>,
    /// MIME type.
    #[serde(default)]
    pub mime_type: String,
    /// Whether the file needs to be organized from a temp directory to a permanent location.
    #[serde(default)]
    pub require_organized: bool,
    /// File size in bytes.
    #[serde(default)]
    pub size_bytes: u64,
    /// SHA-256 hash of the file content.
    #[serde(default)]
    pub sha256: String,
}

/// Text content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TextPart {
    /// Text content.
    pub text: String,
    /// Optional part ID (for cross-event tracing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    /// Content-part identity fact from the producing side (x-agent LlmPartType
    /// variant name, camelCase). Always present on the wire; absent on decode
    /// (legacy data) parses as "default".
    #[serde(default = "default_part_type")]
    pub part_type: String,
}

/// Image content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImagePart {
    /// Image source.
    pub source: FileRef,
    /// Detail hint (low/high/auto).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Part unique identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    /// Content-part identity fact from the producing side (x-agent LlmPartType
    /// variant name, camelCase). Always present on the wire; absent on decode
    /// (legacy data) parses as "default".
    #[serde(default = "default_part_type")]
    pub part_type: String,
}

/// Audio content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioPart {
    /// Audio source.
    pub source: FileRef,
    /// Sample rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// Number of channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    /// Duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Part unique identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    /// Content-part identity fact from the producing side (x-agent LlmPartType
    /// variant name, camelCase). Always present on the wire; absent on decode
    /// (legacy data) parses as "default".
    #[serde(default = "default_part_type")]
    pub part_type: String,
}

/// Video content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VideoPart {
    /// Video source.
    pub source: FileRef,
    /// Duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Frame rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Part unique identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    /// Content-part identity fact from the producing side (x-agent LlmPartType
    /// variant name, camelCase). Always present on the wire; absent on decode
    /// (legacy data) parses as "default".
    #[serde(default = "default_part_type")]
    pub part_type: String,
}

/// File content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FilePart {
    /// File source.
    pub source: FileRef,
    /// Part unique identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    /// Content-part identity fact from the producing side (x-agent LlmPartType
    /// variant name, camelCase). Always present on the wire; absent on decode
    /// (legacy data) parses as "default".
    #[serde(default = "default_part_type")]
    pub part_type: String,
}

/// Unified multimodal content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text(TextPart),
    Image(ImagePart),
    Audio(AudioPart),
    Video(VideoPart),
    File(FilePart),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_file_ref() -> FileRef {
        FileRef {
            remote_url: String::new(),
            local_uri: "/tmp/a.bin".to_string(),
            remote_expires_at: None,
            mime_type: "application/octet-stream".to_string(),
            require_organized: false,
            size_bytes: 1,
            sha256: String::new(),
        }
    }

    #[test]
    fn part_type_roundtrips_with_value_on_every_variant() {
        let parts = vec![
            ContentPart::Text(TextPart {
                text: "hello".to_string(),
                part_id: None,
                part_type: "references".to_string(),
            }),
            ContentPart::Image(ImagePart {
                source: test_file_ref(),
                detail: None,
                width: None,
                height: None,
                part_id: None,
                part_type: "hiddenInstruction".to_string(),
            }),
            ContentPart::Audio(AudioPart {
                source: test_file_ref(),
                sample_rate: None,
                channels: None,
                duration_ms: None,
                part_id: None,
                part_type: "turnMeta".to_string(),
            }),
            ContentPart::Video(VideoPart {
                source: test_file_ref(),
                duration_ms: None,
                fps: None,
                width: None,
                height: None,
                part_id: None,
                part_type: "contextChanges".to_string(),
            }),
            ContentPart::File(FilePart {
                source: test_file_ref(),
                part_id: None,
                part_type: "interruptedContext".to_string(),
            }),
        ];

        for part in parts {
            let json = serde_json::to_value(&part).unwrap();
            let wire_part_type = json
                .get("partType")
                .and_then(|v| v.as_str())
                .expect("wire JSON must always carry partType")
                .to_owned();
            let back: ContentPart = serde_json::from_value(json).unwrap();
            assert_eq!(back, part, "round-trip must preserve part_type");
            let back_part_type = match &back {
                ContentPart::Text(p) => p.part_type.as_str(),
                ContentPart::Image(p) => p.part_type.as_str(),
                ContentPart::Audio(p) => p.part_type.as_str(),
                ContentPart::Video(p) => p.part_type.as_str(),
                ContentPart::File(p) => p.part_type.as_str(),
            };
            assert_eq!(wire_part_type, back_part_type);
        }
    }

    #[test]
    fn part_type_is_always_serialized_even_when_default() {
        let part = TextPart {
            text: "hello".to_string(),
            part_id: None,
            part_type: "default".to_string(),
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(
            json.get("partType").and_then(|v| v.as_str()),
            Some("default"),
            "wire JSON must carry partType even for the default value"
        );
    }

    #[test]
    fn part_type_parses_default_when_absent_on_decode() {
        let json = serde_json::json!({ "text": "legacy" });
        let part: TextPart = serde_json::from_value(json).unwrap();
        assert_eq!(part.part_type, "default", "legacy data parses as default");
    }
}
