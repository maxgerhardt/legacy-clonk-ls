use dashmap::DashMap;
use legacy_clonk_ls::analysis::{diagnostics, index::{self as c4index, Index}, query};
use legacy_clonk_ls::lang::Translation;
use legacy_clonk_ls::lsp::doc::{DocType, Document};
use legacy_clonk_ls::lsp::token_types::TokenTypes;
use legacy_clonk_ls::workspace::{IdInfo, Workspace};
use serde_json::json;
use std::fs::OpenOptions;
use std::sync::RwLock;
use tokio;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use tracing::info;
use tracing::level_filters::LevelFilter;
use tree_sitter::{InputEdit, Point};

struct OwnSemanticTokenType;

impl OwnSemanticTokenType {
    const PARAMETER_TYPE: SemanticTokenType = SemanticTokenType::new("parameterType");
    const ID: SemanticTokenType = SemanticTokenType::new("id");
    const BOOL: SemanticTokenType = SemanticTokenType::new("bool");
    const THIS: SemanticTokenType = SemanticTokenType::new("thisKeyword");
    const CONTROL: SemanticTokenType = SemanticTokenType::new("controlKeyword");
    const PLACEHOLDER: SemanticTokenType = SemanticTokenType::new("formatPlaceholder");
    const ESCAPE: SemanticTokenType = SemanticTokenType::new("stringEscape");
    const TRANSLATION: SemanticTokenType = SemanticTokenType::new("translation");
}
const NEGOTIATE_TOKENT_TYPES: bool = false;
const LOG_TREE_UPDATE_POSITIONS: bool = false;

struct Backend {
    client: Client,
    token_types: RwLock<TokenTypes>,
    documents: DashMap<Url, Document>,
    workspace: RwLock<Workspace>,
    /// C4Script index of all scripts below the workspace roots
    index: RwLock<Index>,
    /// game folder (initializationOptions.gameDir / env C4LS_GAME_DIR) and whether to index all its packs
    game: RwLock<Option<(std::path::PathBuf, bool)>>,
}

impl Backend {
    fn parse_semantic_tokens_capabilities(
        &self,
        _params: &InitializeParams,
    ) -> Option<(TokenTypes, SemanticTokensLegend)> {
        // These are guaranteed by lsp
        let mut negotiated = vec![
            SemanticTokenType::COMMENT,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::MACRO,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::TYPE,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::METHOD,
        ];

        let mut lut = TokenTypes {
            comment: 0,
            string: 1,
            number: 2,
            pragma_strict: 3,
            appendto: 3,
            id: 4,
            var_scope: 4,
            nil: 4,
            keyword: 4,
            parameter: 6,
            method: 7,
            parameter_type: 5,
            bool: 4,
            operator: 0, // TODO
            enum_member: 8,
            this_kw: 4,
            variable: 9,
            property: 10,
            control_kw: 4,
            placeholder: 1,
            escape: 1,
            translation: 3,
        };
        negotiated.push(SemanticTokenType::ENUM_MEMBER); // index 8: engine constants, static consts
        negotiated.push(SemanticTokenType::VARIABLE); // 9: function-local variables
        negotiated.push(SemanticTokenType::PROPERTY); // 10: object locals/statics

        // Set additional custom tokens
        if NEGOTIATE_TOKENT_TYPES {
            if let Some(ref doc) = _params.capabilities.text_document {
                if let Some(ref tt) = doc.semantic_tokens {
                    for t in &tt.token_types {
                        if t == &OwnSemanticTokenType::PARAMETER_TYPE {
                            lut.parameter_type = (negotiated.len()) as u32;
                            negotiated.push(OwnSemanticTokenType::PARAMETER_TYPE);
                        } else if t == &OwnSemanticTokenType::ID {
                            lut.id = (negotiated.len()) as u32;
                            negotiated.push(OwnSemanticTokenType::ID);
                        } else if t == &OwnSemanticTokenType::BOOL {
                            lut.bool = (negotiated.len()) as u32;
                            negotiated.push(OwnSemanticTokenType::BOOL);
                        }
                    }
                } else {
                    return None;
                }
            } else {
                return None;
            }
        } else {
            lut.parameter_type = (negotiated.len()) as u32;
            negotiated.push(OwnSemanticTokenType::PARAMETER_TYPE);
            lut.id = (negotiated.len()) as u32;
            negotiated.push(OwnSemanticTokenType::ID);
            lut.bool = (negotiated.len()) as u32;
            negotiated.push(OwnSemanticTokenType::BOOL);
            lut.this_kw = (negotiated.len()) as u32;
            negotiated.push(OwnSemanticTokenType::THIS);
            lut.control_kw = (negotiated.len()) as u32;
            negotiated.push(OwnSemanticTokenType::CONTROL);
            lut.placeholder = (negotiated.len()) as u32;
            negotiated.push(OwnSemanticTokenType::PLACEHOLDER);
            lut.escape = (negotiated.len()) as u32;
            negotiated.push(OwnSemanticTokenType::ESCAPE);
            lut.translation = (negotiated.len()) as u32;
            negotiated.push(OwnSemanticTokenType::TRANSLATION);
        }

        let legend = SemanticTokensLegend {
            token_types: negotiated,
            token_modifiers: vec![SemanticTokenModifier::DEFAULT_LIBRARY], // bit 0: engine functions
        };

        Some((lut, legend))
    }

    fn add_document(&self, uri: Url, contents: String) -> std::result::Result<(), String> {
        info!("add_document endpoint triggered. uri: {}", uri);

        let doc_type;
        match DocType::from_uri(&uri) {
            Ok(dt) => {
                doc_type = dt;
            }
            Err(e) => {
                let s = e.to_string();
                tracing::error!("could not get doctype from url: {}", &s);
                return Err(s);
            }
        }

        info!("detected doctype for document: {:?}", &doc_type);

        let mut parser = doc_type.get_parser().expect("Could not load language");

        if let Some(tree) = parser.parse(&contents, None) {
            let doc = Document::new(uri.clone(), tree, contents, doc_type);
            self.documents.insert(uri, doc);
            Ok(())
        } else {
            Err(String::from("Could not parse document"))
        }
    }

    fn change_document(&self, uri: &Url, contents: String) -> std::result::Result<(), String> {
        if let Some(ref mut doc) = self.documents.get_mut(&uri) {
            tracing::info!(
                "Changed document {}, having doc type {:?}",
                &uri,
                &doc.doc_type
            );
            let mut parser = doc.doc_type.get_parser().expect("Could not load language");

            let start_byte = doc.tree.root_node().start_byte();
            let old_end_byte = doc.tree.root_node().end_byte();
            let old_end_position = doc.tree.root_node().end_position();
            let new_end_row = contents.chars().filter(|c| c == &'\n').count();
            let new_end_column = contents.chars().rev().position(|c| c == '\n').unwrap_or(0);

            let old_tree = parser.parse(&doc.source, None).unwrap();
            let old_r = old_tree.root_node();
            let new_tree = parser.parse(&contents, None).unwrap();
            let new_r = new_tree.root_node();

            if LOG_TREE_UPDATE_POSITIONS {
                tracing::info!(
                    "         Old tree root positions: start: {}; end: {}; bytes_start: {}; bytes_end: {}",
                    old_r.start_position(),
                    old_r.end_position(),
                    old_r.start_byte(),
                    old_r.end_byte()
                );
                tracing::info!(
                    "Computed old tree root positions: start: {}; end: {}; bytes_start: {}; bytes_end: {}",
                    Point { row: 0, column: 0 },
                    old_end_position,
                    start_byte,
                    old_end_byte,
                );
                tracing::info!(
                    "         New tree root positions: start: {}; end: {}; bytes_start: {}; bytes_end: {}",
                    new_r.start_position(),
                    new_r.end_position(),
                    new_r.start_byte(),
                    new_r.end_byte()
                );
                tracing::info!(
                    "Computed new tree root positions: start: {}; end: {}; bytes_start: {}; bytes_end: {}",
                    Point { row: 0, column: 0 },
                    Point {
                        row: new_end_row,
                        column: new_end_column,
                    },
                    start_byte,
                    contents.len(),
                );
            }

            doc.tree.edit(&InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte: contents.len(),
                start_position: Point { row: 0, column: 0 },
                old_end_position,
                new_end_position: Point {
                    row: new_end_row,
                    column: new_end_column,
                },
            });

            if let Some(new_tree) = parser.parse(&contents, Some(&doc.tree)) {
                doc.tree = new_tree;
                doc.source = contents;
                Ok(())
            } else {
                Err(String::from("Could not update parse tree"))
            }
        } else {
            Err(String::from("Could not get document"))
        }
    }

    /// Re-index an open script and compute its diagnostics.
    fn analyze(&self, uri: &Url) -> Option<Vec<Diagnostic>> {
        let doc = self.documents.get(uri)?;
        if !matches!(doc.doc_type, DocType::Script) {
            return None;
        }
        let path = c4index::uri_to_path(uri)?;
        let info = c4index::extract(&path, &doc.source, &doc.tree);
        let mut index = self.index.write().ok()?;
        let diags = diagnostics::diagnose(&index, &info, &doc.source, &doc.tree);
        index.update_file(info);
        Some(diags)
    }

    async fn refresh(&self, uri: Url) {
        if let Some(diags) = self.analyze(&uri) {
            self.client.publish_diagnostics(uri, diags, None).await;
        }
    }

    /// Run a query against an open script document with fresh index info.
    /// DefCore.txt / Scenario.txt / ActMap.txt: run `f` with the file kind, the index, the file's path and game folder.
    fn with_ini<T>(&self, uri: &Url, f: impl FnOnce(legacy_clonk_ls::analysis::ini::IniFile, &Document, &legacy_clonk_ls::analysis::ini::Ctx) -> T) -> Option<T> {
        use legacy_clonk_ls::analysis::ini::{Ctx, IniFile};
        let doc = self.documents.get(uri)?;
        let file = match doc.doc_type {
            DocType::DefCore => IniFile::DefCore,
            DocType::ActMap => IniFile::ActMap,
            DocType::ScenarioTxt => IniFile::Scenario,
            _ => return None,
        };
        let path = c4index::uri_to_path(uri)?;
        let game = self.game.read().ok().and_then(|g| g.clone()).map(|(d, _)| d);
        let index = self.index.read().ok()?;
        let ctx = Ctx { index: &index, path: &path, game_dir: game.as_deref() };
        Some(f(file, &doc, &ctx))
    }

    fn with_script<T>(&self, uri: &Url, f: impl FnOnce(&Index, &c4index::FileInfo, &Document) -> T) -> Option<T> {
        let doc = self.documents.get(uri)?;
        if !matches!(doc.doc_type, DocType::Script) {
            return None;
        }
        let path = c4index::uri_to_path(uri)?;
        let info = c4index::extract(&path, &doc.source, &doc.tree);
        let index = self.index.read().ok()?;
        Some(f(&index, &info, &doc))
    }

    /// Index the game's packs that aren't indexed yet (Objects.c4d + packs named in scenarios' [Definitions]).
    /// Unpacking with c4group can take a few seconds, so it runs on a blocking thread; open scripts are
    /// re-checked afterwards.
    async fn index_game_packs(&self) {
        let game = self.game.read().ok().and_then(|g| g.clone());
        let (game_dir, all) = match game {
            Some(g) => g,
            None => return,
        };
        let packs = match self.index.read() {
            Ok(i) => legacy_clonk_ls::analysis::index::game_packs_to_index(&i, &game_dir, all),
            Err(_) => return,
        };
        if packs.is_empty() {
            return;
        }
        let result = tokio::task::spawn_blocking(move || legacy_clonk_ls::analysis::index::index_game_packs(&game_dir, &packs)).await;
        if let Ok((packs_index, log)) = result {
            // merge before logging, so "indexed pack X" means X is usable
            if let Ok(mut i) = self.index.write() {
                i.merge(packs_index);
            }
            for l in log {
                tracing::info!("{}", l);
                self.client.log_message(MessageType::INFO, l).await;
            }
            let open: Vec<Url> = self.documents.iter().map(|d| d.key().clone()).collect();
            for uri in open {
                self.refresh(uri).await;
            }
        }
    }

    fn drop_document(&self, uri: &Url) {
        self.documents.remove(uri);
    }

    async fn request_ids(&self) -> Result<serde_json::Value> {
        let ws = self.workspace.read();
        if let Ok(ws) = ws {
            let ids = ws.get_ids();
            Ok(json!(ids))
        } else {
            let v: Vec<IdInfo> = vec![];
            Ok(json!(v))
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.client
            .log_message(MessageType::INFO, "initializing server...")
            .await;

        self.client
            .log_message(MessageType::INFO, "parsed capabilities...")
            .await;

        let mut semantic_tokens_capabilities: Option<SemanticTokensServerCapabilities> = None;

        if let Some((lut, legend)) = self.parse_semantic_tokens_capabilities(&params) {
            semantic_tokens_capabilities = Some(
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                    legend,
                    ..SemanticTokensOptions::default()
                }),
            );

            if let Ok(mut tt) = self.token_types.write() {
                *tt = lut;
            }
        }

        let snippets = params.capabilities.text_document.as_ref()
            .and_then(|t| t.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|i| i.snippet_support)
            .unwrap_or(false);
        legacy_clonk_ls::analysis::complete::SNIPPETS.store(snippets, std::sync::atomic::Ordering::Relaxed);

        if let Some(lang_tag) = params.locale {
            Translation::configure(lang_tag);
        }

        // Index all Clonk scripts below the workspace roots (+ initializationOptions.extraRoots, env C4LS_EXTRA_ROOTS)
        let mut roots: Vec<std::path::PathBuf> = vec![];
        #[allow(deprecated)]
        if let Some(uri) = &params.root_uri {
            roots.extend(uri.to_file_path().ok());
        }
        for wf in params.workspace_folders.iter().flatten() {
            roots.extend(wf.uri.to_file_path().ok());
        }
        if let Some(opts) = &params.initialization_options {
            for r in opts.get("extraRoots").and_then(|v| v.as_array()).into_iter().flatten() {
                roots.extend(r.as_str().map(std::path::PathBuf::from));
            }
        }
        let mut game_dir = std::env::var_os("C4LS_GAME_DIR").filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
        let mut all_packs = false;
        if let Some(opts) = &params.initialization_options {
            if let Some(g) = opts.get("gameDir").and_then(|v| v.as_str()) {
                game_dir = Some(std::path::PathBuf::from(g));
            }
            all_packs = opts.get("allPacks").and_then(|v| v.as_bool()).unwrap_or(false);
            if let Some(langs) = opts.get("stringTableLanguages").and_then(|v| v.as_array()) {
                legacy_clonk_ls::analysis::strings::set_languages(langs.iter().filter_map(|l| l.as_str().map(|s| s.to_string())).collect());
            }
            if let Some(tc) = opts.get("typeChecks").and_then(|v| v.as_bool()) {
                legacy_clonk_ls::analysis::diagnostics::TYPE_CHECKS.store(tc, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(ft) = opts.get("fieldTypes").and_then(|v| v.as_bool()) {
                legacy_clonk_ls::analysis::diagnostics::FIELD_TYPES.store(ft, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(oi) = opts.get("objectIds").and_then(|v| v.as_bool()) {
                legacy_clonk_ls::analysis::diagnostics::OBJECT_IDS.store(oi, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(ac) = opts.get("assumeCreateSucceeds").and_then(|v| v.as_bool()) {
                legacy_clonk_ls::analysis::diagnostics::ASSUME_CREATE.store(ac, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(ci) = opts.get("callerInference").and_then(|v| v.as_bool()) {
                legacy_clonk_ls::analysis::diagnostics::CALLER_INFERENCE.store(ci, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(l) = opts.get("lint").and_then(|v| v.as_bool()) {
                legacy_clonk_ls::analysis::diagnostics::LINT.store(l, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(nc) = opts.get("nilChecks").and_then(|v| v.as_bool()) {
                legacy_clonk_ls::analysis::diagnostics::NIL_CHECKS.store(nc, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Ok(mut g) = self.game.write() {
            *g = game_dir.map(|d| (d, all_packs));
        }
        if let Ok(extra) = std::env::var("C4LS_EXTRA_ROOTS") {
            roots.extend(extra.split(';').filter(|s| !s.is_empty()).map(std::path::PathBuf::from));
        }
        if roots.is_empty() {
            roots.extend(std::env::current_dir().ok());
        }
        #[allow(deprecated)]
        let (root_uri, folders) = (params.root_uri.clone(), params.workspace_folders.clone());
        let msg_roots = format!(
            "index roots: {:?} (rootUri: {:?}, workspaceFolders: {:?}, cwd: {:?})",
            roots, root_uri.map(|u| u.to_string()), folders.map(|f| f.iter().map(|x| x.uri.to_string()).collect::<Vec<_>>()),
            std::env::current_dir().ok()
        );
        tracing::info!("{}", msg_roots);
        let index = Index::build(roots);
        let msg = format!("{}; indexed {} scripts, {} definitions", msg_roots, index.files.len(), index.ids.len());
        tracing::info!("{}", msg);
        self.client.log_message(MessageType::INFO, msg).await;
        if let Ok(mut i) = self.index.write() {
            *i = index;
        }

        let text_document_sync_capabilities =
            TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                semantic_tokens_provider: semantic_tokens_capabilities,
                text_document_sync: Some(text_document_sync_capabilities),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    ..Default::default()
                }),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some([">", ".", "#", "$", "~", "[", "=", "|", ";", "(", ","].iter().map(|s| s.to_string()).collect()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let Err(e) = self.add_document(params.text_document.uri, params.text_document.text) {
            self.client.log_message(MessageType::INFO, e).await;
            return;
        }
        // A script outside the indexed roots: index its scenario/definition folder (and the packs it loads)
        let newly = uri.to_file_path().ok().and_then(|p| self.index.write().ok().and_then(|mut i| i.cover(&p)));
        if let Some(dir) = newly {
            let msg = format!("indexed {} (outside the workspace roots)", dir.display());
            tracing::info!("{}", msg);
            self.client.log_message(MessageType::INFO, msg).await;
            self.index_game_packs().await;
        }
        self.refresh(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        legacy_clonk_ls::analysis::types::clear_cache(); // inferred return types may depend on this file

        if let Err(e) = self.change_document(&uri, params.content_changes[0].text.clone()) {
            tracing::error!("Error when updating text document ({:?}): {}", &uri, e);
            self.client.log_message(MessageType::INFO, e).await;
            return;
        }
        self.refresh(uri).await;
    }

    async fn goto_definition(&self, params: GotoDefinitionParams) -> Result<Option<GotoDefinitionResponse>> {
        let p = params.text_document_position_params;
        let locs = self.with_script(&p.text_document.uri, |idx, info, doc| query::definition(idx, info, &doc.tree, &doc.source, p.position));
        Ok(locs.filter(|l| !l.is_empty()).map(GotoDefinitionResponse::Array))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let p = params.text_document_position;
        let decl = params.context.include_declaration;
        Ok(self.with_script(&p.text_document.uri, |idx, info, doc| query::references(idx, info, &doc.tree, &doc.source, p.position, decl)))
    }

    async fn document_symbol(&self, params: DocumentSymbolParams) -> Result<Option<DocumentSymbolResponse>> {
        Ok(self
            .with_script(&params.text_document.uri, |_, info, _| query::document_symbols(info))
            .map(DocumentSymbolResponse::Nested))
    }

    async fn symbol(&self, params: WorkspaceSymbolParams) -> Result<Option<Vec<SymbolInformation>>> {
        let index = match self.index.read() {
            Ok(i) => i,
            Err(_) => return Ok(None),
        };
        Ok(Some(query::workspace_symbols(&index, &params.query)))
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.drop_document(&params.text_document.uri);
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        match self.documents.get(&uri) {
            Some(doc) => {
                // Get as ref?
                let lut = match self.token_types.read() {
                    Ok(lut) => *lut,
                    _ => {
                        tracing::info!(
                            "Could not acquired negotiated token_types. Using default ones instead"
                        );
                        TokenTypes::default()
                    }
                };

                tracing::info!("doc: {}", &doc.source.len());
                let handler = doc.doc_type.get_handler();
                let tokens = handler.collect_semantic_tokens(&doc.tree, lut, doc.source.as_ref());
                Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: tokens,
                })))
            }
            _ => {
                tracing::info!("Requested semantic_tokens_full endpoint, but did not found document with uri: {}", &uri);
                Ok(None)
            }
        }
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "server initialized")
            .await;
        self.index_game_packs().await;
    }

    async fn shutdown(&self) -> Result<()> {
        self.client
            .log_message(MessageType::INFO, "shutting down...")
            .await;

        Ok(())
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        match self.documents.get(&uri) {
            Some(doc) => {
                if matches!(doc.doc_type, DocType::DefCore | DocType::ActMap | DocType::ScenarioTxt) {
                    drop(doc);
                    let pos = params.text_document_position.position;
                    let items = self.with_ini(&uri, |file, doc, ctx| legacy_clonk_ls::analysis::ini::complete(file, &doc.source, pos, ctx));
                    return Ok(items.flatten().map(CompletionResponse::Array));
                }
                if matches!(doc.doc_type, DocType::Script) {
                    drop(doc);
                    let pos = params.text_document_position.position;
                    let trigger = params.context.as_ref().and_then(|c| c.trigger_character.clone());
                    let items = self.with_script(&uri, |idx, info, doc| {
                        legacy_clonk_ls::analysis::complete::complete(idx, info, &doc.source, pos, trigger.as_deref())
                    });
                    return Ok(items.flatten().map(CompletionResponse::Array));
                }
                let handler = doc.doc_type.get_handler();
                let cursor_pos = params.text_document_position.position;
                let items = handler.get_completions(&doc.tree, &doc.source, cursor_pos);

                Ok(items.map(|i| CompletionResponse::Array(i)))
            }
            None => Ok(None),
        }
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        Ok(self
            .with_script(&uri, |idx, info, doc| legacy_clonk_ls::analysis::complete::signature_help(idx, info, &doc.source, pos))
            .flatten())
    }

    /// Format Document: c4format with the rules of the nearest .c4format (one edit replacing the whole text).
    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let Some(doc) = self.documents.get(&uri) else { return Ok(None) };
        if !matches!(doc.doc_type, DocType::Script) {
            return Ok(None);
        }
        let src = doc.source.clone();
        drop(doc);
        let Some(path) = c4index::uri_to_path(&uri) else { return Ok(None) };
        let style = match legacy_clonk_ls::analysis::format::Style::for_file(&path) {
            Ok((s, _)) => s,
            Err(e) => {
                self.client.show_message(MessageType::ERROR, format!("c4format: {}", e)).await;
                return Ok(None);
            }
        };
        match legacy_clonk_ls::analysis::format::format_explain(&src, &style) {
            Ok(text) if text != src => {
                let lines = src.split('\n').count() as u32;
                Ok(Some(vec![TextEdit { range: Range::new(Position::new(0, 0), Position::new(lines, 0)), new_text: text }]))
            }
            Ok(_) => Ok(Some(vec![])),
            Err(reason) => {
                self.client.show_message(MessageType::INFO, format!("c4format: left unchanged ({})", reason)).await;
                Ok(None)
            }
        }
    }

    /// Quick fixes: suppress a diagnostic with `// NOLINT(code)` at the end of its line (or `// NOLINTNEXTLINE(code)`
    /// above it when the line already ends in a comment), see analysis/lint.rs.
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let Some(source) = self.documents.get(&uri).map(|d| d.source.clone()) else { return Ok(None) };
        let lines: Vec<&str> = source.split('\n').collect();
        let mut actions: CodeActionResponse = vec![];
        for d in params.context.diagnostics.iter().filter(|d| d.source.as_deref() == Some("c4script")) {
            let Some(NumberOrString::String(code)) = &d.code else { continue };
            let row = d.range.start.line as usize;
            let Some(line) = lines.get(row) else { continue };
            let line = line.trim_end_matches('\r');
            let edit = if line.contains("//") || line.trim_end().ends_with("*/") {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                TextEdit { range: Range::new(Position::new(row as u32, 0), Position::new(row as u32, 0)), new_text: format!("{}// NOLINTNEXTLINE({})\n", indent, code) }
            } else {
                let end = line.encode_utf16().count() as u32;
                TextEdit { range: Range::new(Position::new(row as u32, end), Position::new(row as u32, end)), new_text: format!(" // NOLINT({})", code) }
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri.clone(), vec![edit]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Suppress {} on this line (// NOLINT)", code),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![d.clone()]),
                edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                ..Default::default()
            }));
        }
        Ok(Some(actions))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        if let Some(text) = self.with_ini(&uri, |file, doc, ctx| legacy_clonk_ls::analysis::ini::hover(file, &doc.source, pos, ctx)).flatten() {
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value: text }),
                range: None,
            }));
        }
        if let Some(text) = self.with_script(&uri, |idx, info, doc| query::hover(idx, info, &doc.tree, &doc.source, pos)).flatten() {
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value: text }),
                range: None,
            }));
        }
        match self.documents.get(&uri) {
            Some(doc) => {
                let handler = doc.doc_type.get_handler();
                if let Some(text) =
                    handler.get_hover_text(&doc, params.text_document_position_params.position)
                {
                    let markup = MarkupContent {
                        value: text,
                        kind: MarkupKind::Markdown,
                    };

                    let contents = HoverContents::Markup(markup);
                    let response = Hover {
                        contents,
                        range: None,
                    };

                    return Ok(Some(response));
                }
            }
            _ => {
                tracing::info!(
                    "Requested hover endpoint, but did not found document with uri: {}",
                    &uri
                );
            }
        }

        Ok(None)
    }
}

async fn start_language_server() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::build(|client| Backend {
        token_types: RwLock::new(TokenTypes::default()),
        client,
        documents: DashMap::new(),
        workspace: RwLock::new(Workspace::new()),
        index: RwLock::new(Index::default()),
        game: RwLock::new(None),
    })
    .custom_method("x-legacy-clonk/getAllIds", Backend::request_ids)
    .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);

    let mut debug_mode = false;

    loop {
        if let Some(arg) = args.next() {
            match arg.as_ref() {
                "--debug" => {
                    debug_mode = true;
                }
                _ => {}
            }
        } else {
            break;
        }
    }

    let log_path = std::env::var("LC_LSP_LOG_PATH").ok();

    if let Some(log_path) = log_path {
        let options = OpenOptions::new()
            .append(true)
            .create(true)
            .open(log_path)
            .unwrap();

        let level_filter = if debug_mode {
            LevelFilter::TRACE
        } else {
            LevelFilter::INFO
        };
        let subscriber = tracing_subscriber::fmt()
            .with_writer(options)
            .with_file(true)
            .with_max_level(level_filter)
            .finish();

        tracing::subscriber::set_global_default(subscriber).unwrap();
    }

    start_language_server().await;
}
