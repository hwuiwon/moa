//! Out-of-line tests for gateway outbound text limits.

mod support;

use moa_gateway::{
    DISCORD_MAX_MESSAGE_LENGTH, DiscordRenderer, SLACK_MAX_MESSAGE_LENGTH, SlackRenderer,
    TELEGRAM_MAX_MESSAGE_LENGTH, TelegramRenderer,
};
use support::{assert_grapheme_chunks, text_message};

#[test]
fn telegram_outbound_text_exceeding_4096_chars_is_split_at_grapheme_boundary() {
    let text = format!(
        "{}👋{}",
        "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH),
        "b".repeat(32)
    );
    let chunks = TelegramRenderer::new()
        .render(&text_message(text.clone()))
        .into_iter()
        .map(|chunk| chunk.text)
        .collect::<Vec<_>>();

    assert_grapheme_chunks(&chunks, &text, TELEGRAM_MAX_MESSAGE_LENGTH);
}

#[test]
fn slack_outbound_text_exceeding_40000_chars_is_split_at_grapheme_boundary() {
    let text = format!(
        "{}👋{}",
        "a".repeat(SLACK_MAX_MESSAGE_LENGTH),
        "b".repeat(32)
    );
    let chunks = SlackRenderer::new()
        .render(&text_message(text.clone()))
        .into_iter()
        .map(|chunk| chunk.text)
        .collect::<Vec<_>>();

    assert_grapheme_chunks(&chunks, &text, SLACK_MAX_MESSAGE_LENGTH);
}

#[test]
fn discord_outbound_text_exceeding_2000_chars_is_split_at_grapheme_boundary() {
    let text = format!(
        "{}👋{}",
        "a".repeat(DISCORD_MAX_MESSAGE_LENGTH),
        "b".repeat(32)
    );
    let chunks = DiscordRenderer::new()
        .render(&text_message(text.clone()))
        .into_iter()
        .map(|chunk| {
            chunk
                .embed_description
                .expect("discord text render should use embed descriptions")
        })
        .collect::<Vec<_>>();

    assert_grapheme_chunks(&chunks, &text, DISCORD_MAX_MESSAGE_LENGTH);
}

#[test]
fn outbound_text_with_4byte_emoji_at_truncation_boundary_does_not_split_emoji() {
    let family = "👨‍👩‍👧";
    let text = format!(
        "{}{family}{}",
        "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH - 1),
        "b".repeat(16)
    );
    let chunks = TelegramRenderer::new()
        .render(&text_message(text.clone()))
        .into_iter()
        .map(|chunk| chunk.text)
        .collect::<Vec<_>>();

    assert_grapheme_chunks(&chunks, &text, TELEGRAM_MAX_MESSAGE_LENGTH);
    let emoji_chunks = chunks
        .iter()
        .filter(|chunk| {
            chunk.contains('👨')
                || chunk.contains('👩')
                || chunk.contains('👧')
                || chunk.contains('\u{200d}')
        })
        .collect::<Vec<_>>();
    assert_eq!(
        emoji_chunks.len(),
        1,
        "multi-codepoint emoji should stay entirely in one rendered chunk"
    );
    assert!(
        emoji_chunks[0].contains(family),
        "rendered chunk contains only part of the family emoji: {}",
        emoji_chunks[0]
    );
}

#[test]
fn outbound_text_under_limit_returns_single_message_unchanged() {
    let text = "under limit 👋";
    let telegram = TelegramRenderer::new().render(&text_message(text));
    let slack = SlackRenderer::new().render(&text_message(text));
    let discord = DiscordRenderer::new().render(&text_message(text));

    assert_eq!(telegram.len(), 1);
    assert_eq!(telegram[0].text, text);
    assert_eq!(slack.len(), 1);
    assert_eq!(slack[0].text, text);
    assert_eq!(discord.len(), 1);
    assert_eq!(discord[0].embed_description.as_deref(), Some(text));
}
