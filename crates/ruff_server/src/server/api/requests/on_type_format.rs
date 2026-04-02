use anyhow::Context;
use lsp_types::{self as types, request as req};
use ruff_python_ast::str_prefix::{AnyStringPrefix, StringLiteralPrefix};
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::{SourceType, StringFlags};
use ruff_python_parser::{ParseOptions, parse_unchecked};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::edit::{RangeExt, ToRangeExt};
use crate::resolve::is_document_excluded_for_formatting;
use crate::server::Result;
use crate::session::{Client, DocumentSnapshot};

pub(crate) struct FormatOnType;

impl super::RequestHandler for FormatOnType {
    type RequestType = req::OnTypeFormatting;
}

impl super::BackgroundDocumentRequestHandler for FormatOnType {
    fn document_url(
        params: &types::DocumentOnTypeFormattingParams,
    ) -> std::borrow::Cow<'_, lsp_types::Url> {
        std::borrow::Cow::Borrowed(&params.text_document_position.text_document.uri)
    }

    fn run_with_snapshot(
        snapshot: DocumentSnapshot,
        _client: &Client,
        params: types::DocumentOnTypeFormattingParams,
    ) -> Result<super::FormatResponse> {
        format_on_type(&snapshot, &params)
    }
}

fn format_on_type(
    snapshot: &DocumentSnapshot,
    params: &types::DocumentOnTypeFormattingParams,
) -> Result<super::FormatResponse> {
    if params.ch != "{" {
        return Ok(None);
    }

    let SourceType::Python(source_type) = snapshot.query().source_type() else {
        return Ok(None);
    };

    if !snapshot
        .query()
        .settings()
        .formatter
        .f_string_conversion_on_type
    {
        return Ok(None);
    }

    let text_document = snapshot
        .query()
        .as_single_document()
        .context("Failed to get text document for the on-type formatting request")
        .unwrap();
    let settings = snapshot.query().settings();
    let file_path = snapshot.query().virtual_file_path();

    if is_document_excluded_for_formatting(
        &file_path,
        &settings.file_resolver,
        &settings.formatter,
        text_document.language_id(),
    ) {
        return Ok(None);
    }

    let source = text_document.contents();
    let cursor_offset = types::Range::new(
        params.text_document_position.position,
        params.text_document_position.position,
    )
    .to_text_range(source, text_document.index(), snapshot.encoding())
    .start();

    if cursor_offset == TextSize::default() {
        return Ok(None);
    }

    let typed_offset = cursor_offset - TextSize::new(1);
    if source.as_bytes().get(usize::from(typed_offset)) != Some(&b'{') {
        return Ok(None);
    }

    let parsed = parse_unchecked(source, ParseOptions::from(source_type));
    let token = parsed
        .tokens()
        .iter()
        .find(|token| token.kind() == TokenKind::String && token.range().contains(typed_offset));

    let Some(token) = token else {
        return Ok(None);
    };

    let flags = token
        .string_flags()
        .expect("regular string token should expose string flags");

    if !matches!(
        flags.prefix(),
        AnyStringPrefix::Regular(StringLiteralPrefix::Empty | StringLiteralPrefix::Raw { .. })
    ) {
        return Ok(None);
    }

    let contents_range = TextRange::new(
        token.start() + flags.opener_len(),
        token.end() - flags.closer_len(),
    );
    if !contents_range.contains(typed_offset) {
        return Ok(None);
    }

    let before_typed_brace = &source[TextRange::new(contents_range.start(), typed_offset)];
    let after_typed_brace = &source[TextRange::new(cursor_offset, contents_range.end())];
    if contains_braces(before_typed_brace)
        || contains_braces(after_typed_brace)
        || ends_with_backslash_sequence(before_typed_brace)
    {
        return Ok(None);
    }

    Ok(Some(vec![types::TextEdit {
        range: TextRange::empty(token.start()).to_range(
            source,
            text_document.index(),
            snapshot.encoding(),
        ),
        new_text: "f".to_string(),
    }]))
}

fn contains_braces(text: &str) -> bool {
    text.bytes().any(|byte| matches!(byte, b'{' | b'}'))
}

fn ends_with_backslash_sequence(text: &str) -> bool {
    text.ends_with('\\') || text.ends_with("\\N")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use lsp_types::{
        ClientCapabilities, FormattingOptions, Position, TextDocumentContentChangeEvent,
        TextDocumentPositionParams, Url,
    };

    use crate::session::{Client, GlobalOptions};
    use crate::{PositionEncoding, TextDocument, Workspace, Workspaces};

    use super::*;

    struct TestWorkspace {
        path: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ruff-on-type-format-{unique}-{}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct TestContext {
        _workspace_dir: TestWorkspace,
        session: crate::Session,
        file_url: Url,
    }

    fn create_session(
        file_name: &str,
        language_id: &str,
        content: &str,
        config: Option<&str>,
    ) -> TestContext {
        let (main_loop_sender, _) = crossbeam::channel::unbounded();
        let (client_sender, _) = crossbeam::channel::unbounded();
        let client = Client::new(main_loop_sender, client_sender);

        let workspace_dir = TestWorkspace::new();
        let workspace_url = Url::from_file_path(&workspace_dir.path).unwrap();

        if let Some(config) = config {
            fs::write(workspace_dir.path.join("ruff.toml"), config).unwrap();
        }

        let options = GlobalOptions::default();
        let global = options.into_settings(client.clone());

        let mut session = crate::Session::new(
            &ClientCapabilities::default(),
            PositionEncoding::UTF16,
            global,
            &Workspaces::new(vec![
                Workspace::new(workspace_url).with_options(crate::ClientOptions::default()),
            ]),
            &client,
        )
        .unwrap();

        let file_url = Url::from_file_path(workspace_dir.path.join(file_name)).unwrap();
        let document = TextDocument::new(content.to_string(), 0).with_language_id(language_id);
        session.open_text_document(file_url.clone(), document);

        TestContext {
            _workspace_dir: workspace_dir,
            session,
            file_url,
        }
    }

    fn apply_edits(source: &str, edits: Vec<types::TextEdit>) -> String {
        let mut document = TextDocument::new(source.to_string(), 0);
        document.apply_changes(
            edits
                .into_iter()
                .map(|edit| TextDocumentContentChangeEvent {
                    range: Some(edit.range),
                    range_length: None,
                    text: edit.new_text,
                })
                .collect(),
            1,
            PositionEncoding::UTF16,
        );
        document.into_contents()
    }

    fn on_type_params(
        file_url: Url,
        line: u32,
        character: u32,
    ) -> types::DocumentOnTypeFormattingParams {
        types::DocumentOnTypeFormattingParams {
            text_document_position: TextDocumentPositionParams {
                text_document: types::TextDocumentIdentifier { uri: file_url },
                position: Position { line, character },
            },
            ch: "{".to_string(),
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                properties: Default::default(),
                trim_trailing_whitespace: None,
                insert_final_newline: None,
                trim_final_newlines: None,
            },
        }
    }

    fn on_type_params_for_offset(
        file_url: Url,
        line: u32,
        typed_offset: usize,
    ) -> types::DocumentOnTypeFormattingParams {
        on_type_params(file_url, line, u32::try_from(typed_offset + 1).unwrap())
    }

    #[test]
    fn converts_regular_string_to_f_string() {
        let source = "message = \"Hello {\"\n";
        let context = create_session(
            "test.py",
            "python",
            source,
            Some("[format]\nf-string-conversion-on-type = true\n"),
        );
        let snapshot = context
            .session
            .take_snapshot(context.file_url.clone())
            .unwrap();

        let edits = format_on_type(
            &snapshot,
            &on_type_params_for_offset(context.file_url, 0, source.rfind('{').unwrap()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(apply_edits(source, edits), "message = f\"Hello {\"\n");
    }

    #[test]
    fn leaves_strings_unchanged_when_feature_is_disabled() {
        let source = "message = \"Hello {\"\n";
        let context = create_session("test.py", "python", source, None);
        let snapshot = context
            .session
            .take_snapshot(context.file_url.clone())
            .unwrap();

        assert_eq!(
            format_on_type(
                &snapshot,
                &on_type_params_for_offset(context.file_url, 0, source.rfind('{').unwrap()),
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn leaves_existing_f_strings_unchanged() {
        let source = "message = f\"Hello {\"\n";
        let context = create_session(
            "test.py",
            "python",
            source,
            Some("[format]\nf-string-conversion-on-type = true\n"),
        );
        let snapshot = context
            .session
            .take_snapshot(context.file_url.clone())
            .unwrap();

        assert_eq!(
            format_on_type(
                &snapshot,
                &on_type_params_for_offset(context.file_url, 0, source.rfind('{').unwrap()),
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn does_not_convert_if_string_already_contains_braces() {
        let source = "message = \"{} {\"\n";
        let context = create_session(
            "test.py",
            "python",
            source,
            Some("[format]\nf-string-conversion-on-type = true\n"),
        );
        let snapshot = context
            .session
            .take_snapshot(context.file_url.clone())
            .unwrap();

        assert_eq!(
            format_on_type(
                &snapshot,
                &on_type_params_for_offset(context.file_url, 0, source.rfind('{').unwrap()),
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn does_not_convert_escaped_braces() {
        let source = "message = \"\\{\"\n";
        let context = create_session(
            "test.py",
            "python",
            source,
            Some("[format]\nf-string-conversion-on-type = true\n"),
        );
        let snapshot = context
            .session
            .take_snapshot(context.file_url.clone())
            .unwrap();

        assert_eq!(
            format_on_type(
                &snapshot,
                &on_type_params_for_offset(context.file_url, 0, source.rfind('{').unwrap()),
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn does_not_convert_named_unicode_escape_start() {
        let source = "message = r\"\\N{\"\n";
        let context = create_session(
            "test.py",
            "python",
            source,
            Some("[format]\nf-string-conversion-on-type = true\n"),
        );
        let snapshot = context
            .session
            .take_snapshot(context.file_url.clone())
            .unwrap();

        assert_eq!(
            format_on_type(
                &snapshot,
                &on_type_params_for_offset(context.file_url, 0, source.rfind('{').unwrap()),
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn ignores_braces_outside_strings() {
        let source = "mapping = {\n";
        let context = create_session(
            "test.py",
            "python",
            source,
            Some("[format]\nf-string-conversion-on-type = true\n"),
        );
        let snapshot = context
            .session
            .take_snapshot(context.file_url.clone())
            .unwrap();

        assert_eq!(
            format_on_type(
                &snapshot,
                &on_type_params_for_offset(context.file_url, 0, source.rfind('{').unwrap()),
            )
            .unwrap(),
            None
        );
    }
}
