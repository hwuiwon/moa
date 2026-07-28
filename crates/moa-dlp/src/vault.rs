//! Request-scoped DLP token vault with provenance-aware restoration.

use std::collections::HashMap;

use moa_memory_pii::{PiiCategory, PiiSpan};
use rand::{RngCore, rngs::OsRng};
use secrecy::{ExposeSecret, SecretString};

use crate::error::{Error, Result};

/// Opening delimiter reserved for MOA DLP tokens.
///
/// Defined in `moa-memory-pii` so the irreversible learning sanitizer, which
/// must refuse text carrying a reversible token, can name the same delimiter
/// without the dependency edge running backwards.
pub const TOKEN_OPEN: char = moa_memory_pii::sanitized::RESERVED_DLP_TOKEN_OPEN;

/// Closing delimiter reserved for MOA DLP tokens.
pub const TOKEN_CLOSE: char = moa_memory_pii::sanitized::RESERVED_DLP_TOKEN_CLOSE;

/// Visibility of the source that introduced a protected value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenVisibility {
    /// Caller-visible data may be restored to visible output or tool arguments.
    Visible,
    /// Internal data must never be reconstructed outside its source boundary.
    Hidden,
}

/// Role of the message that introduced a protected value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenSourceRole {
    /// System-authored prompt content.
    System,
    /// User-authored content.
    User,
    /// Assistant-authored content.
    Assistant,
    /// Tool-authored content.
    Tool,
}

/// Destination where a provider-emitted token would be restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenDestination {
    /// Text or summaries visible to the caller.
    VisibleOutput,
    /// Model-generated arguments that will be sent to a tool.
    ToolArgument,
}

/// Provenance recorded for every protected value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenSource {
    visibility: TokenVisibility,
    role: TokenSourceRole,
    field: String,
}

impl TokenSource {
    /// Creates provenance for one structural request field.
    pub fn new(
        visibility: TokenVisibility,
        role: TokenSourceRole,
        field: impl Into<String>,
    ) -> Self {
        Self {
            visibility,
            role,
            field: field.into(),
        }
    }

    /// Returns the source visibility.
    #[must_use]
    pub const fn visibility(&self) -> TokenVisibility {
        self.visibility
    }

    /// Returns the source message role.
    #[must_use]
    pub const fn role(&self) -> TokenSourceRole {
        self.role
    }

    /// Returns the structural source field.
    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }
}

struct VaultEntry {
    original: SecretString,
    source: TokenSource,
    allowed_visible_output: bool,
    allowed_tool_argument: bool,
}

/// Reversible mapping from randomized tokens to their protected originals.
///
/// A vault belongs to exactly one completion request. It is never persisted or
/// shared across pods; correctness comes from keeping it in the request task.
pub struct TokenVault {
    namespace: String,
    entries: HashMap<String, VaultEntry>,
    counter: usize,
}

impl TokenVault {
    /// Creates an empty vault with a cryptographically random 128-bit namespace.
    pub fn new() -> Result<Self> {
        let mut namespace = [0_u8; 16];
        OsRng
            .try_fill_bytes(&mut namespace)
            .map_err(|_| Error::EntropyUnavailable)?;
        let namespace = namespace
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join("");
        Ok(Self {
            namespace,
            entries: HashMap::new(),
            counter: 0,
        })
    }

    /// Replaces every validated span and records its source provenance atomically.
    ///
    /// Validation completes before this vault is mutated. Any malformed,
    /// overlapping, or reserved-syntax input therefore fails without partially
    /// recording values or advancing the token counter.
    pub fn tokenize(
        &mut self,
        text: &str,
        spans: &[PiiSpan],
        source: TokenSource,
    ) -> Result<String> {
        if text.contains(TOKEN_OPEN) || text.contains(TOKEN_CLOSE) {
            return Err(Error::LiteralTokenSyntax);
        }

        let mut selected = spans.iter().collect::<Vec<_>>();
        selected.sort_by_key(|span| (span.start, span.end));
        let mut cursor = 0;
        for span in &selected {
            if span.start > span.end {
                return Err(Error::ReversedSpan {
                    start: span.start,
                    end: span.end,
                });
            }
            if span.start == span.end {
                return Err(Error::EmptySpan { start: span.start });
            }
            if span.end > text.len() {
                return Err(Error::SpanOutOfBounds {
                    start: span.start,
                    end: span.end,
                    text_len: text.len(),
                });
            }
            if !text.is_char_boundary(span.start) || !text.is_char_boundary(span.end) {
                return Err(Error::NonUtf8Boundary {
                    start: span.start,
                    end: span.end,
                });
            }
            if span.start < cursor {
                return Err(Error::OverlappingSpans { start: span.start });
            }
            cursor = span.end;
        }

        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for span in selected {
            out.push_str(&text[cursor..span.start]);
            let original = &text[span.start..span.end];
            out.push_str(&self.token_for(original, span.category, &source));
            cursor = span.end;
        }
        out.push_str(&text[cursor..]);
        Ok(out)
    }

    /// Resolves known tokens according to their provenance and destination.
    ///
    /// Hidden values are redacted in visible output and rejected in tool
    /// arguments. Unknown tokens remain untouched.
    pub fn restore(&self, text: &str, destination: TokenDestination) -> Result<String> {
        if self.entries.is_empty() || !text.contains(TOKEN_OPEN) {
            return Ok(text.to_string());
        }

        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(open) = rest.find(TOKEN_OPEN) {
            out.push_str(&rest[..open]);
            let tail = &rest[open..];
            let Some(close) = tail.find(TOKEN_CLOSE) else {
                out.push_str(tail);
                return Ok(out);
            };
            let end = close + TOKEN_CLOSE.len_utf8();
            let candidate = &tail[..end];
            match self.entries.get(candidate) {
                Some(entry)
                    if destination == TokenDestination::VisibleOutput
                        && entry.allowed_visible_output =>
                {
                    out.push_str(entry.original.expose_secret());
                }
                Some(entry)
                    if destination == TokenDestination::ToolArgument
                        && entry.allowed_tool_argument =>
                {
                    out.push_str(entry.original.expose_secret());
                }
                Some(_) if destination == TokenDestination::VisibleOutput => {
                    out.push_str("[REDACTED]");
                }
                Some(entry) => {
                    return Err(Error::DestinationDenied {
                        role: entry.source.role,
                        field: entry.source.field.clone(),
                        destination,
                    });
                }
                None => out.push_str(candidate),
            }
            rest = &tail[end..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Masks already-minted tokens before residual classification.
    #[must_use]
    pub fn classification_view(&self, text: &str) -> String {
        let mut masked = text.to_string();
        for token in self.entries.keys() {
            masked = masked.replace(token, "[DLP_TOKEN]");
        }
        masked
    }

    /// Returns the number of distinct source/value bindings in the vault.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Reports whether no values have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn token_for(&mut self, original: &str, category: PiiCategory, source: &TokenSource) -> String {
        if let Some(existing) = self.existing_token(original, source) {
            return existing;
        }
        self.counter += 1;
        let token = format!(
            "{TOKEN_OPEN}MOA_DLP_{}_{}_{}{TOKEN_CLOSE}",
            self.namespace,
            category.field_name().to_ascii_uppercase(),
            self.counter
        );
        self.entries.insert(
            token.clone(),
            VaultEntry {
                original: SecretString::new(original.to_owned().into_boxed_str()),
                source: source.clone(),
                allowed_visible_output: source.visibility == TokenVisibility::Visible,
                allowed_tool_argument: source.visibility == TokenVisibility::Visible,
            },
        );
        tracing::trace!(category = category.field_name(), "DLP tokenized span");
        token
    }

    fn existing_token(&self, original: &str, source: &TokenSource) -> Option<String> {
        self.entries.iter().find_map(|(token, entry)| {
            (entry.source == *source && entry.original.expose_secret() == original)
                .then(|| token.clone())
        })
    }
}

impl std::fmt::Debug for TokenVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenVault")
            .field("entries", &self.entries.len())
            .field("counter", &self.counter)
            .field("originals", &"<redacted>")
            .finish()
    }
}
