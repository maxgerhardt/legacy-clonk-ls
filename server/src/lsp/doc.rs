use super::{
    act_map_handler::ActMapHandler, asset_handler::AssetHandler, def_core_handler::DefCoreHandler, particle_handler::ParticleHandler, scenario_txt_handler::ScenarioTxtHandler, script_handler::ScriptHandler
};
use crate::core::kind::NODE_KIND_FN_DEF;
use anyhow::{anyhow, Context}; use tower_lsp::lsp_types::{Position, Url};
use tracing::info;
use tree_sitter::{Language, Point, Tree};

pub enum QueryableItem {
    Function(String),
    Constant(String),
    Unused,
}

#[derive(Debug)]
pub struct Document {
    #[allow(dead_code)]
    pub url: Url,
    pub tree: Tree,
    pub source: String,
    pub doc_type: DocType,
}

impl Document {
    pub fn new(url: Url, tree: Tree, source: String, doc_type: DocType) -> Self {
        Document {
            url,
            tree,
            source,
            doc_type,
        }
    }

    pub fn get_node_at_pos(
        &self,
        pos: tower_lsp::lsp_types::Position,
    ) -> Option<tree_sitter::Node<'_>> {
        let mut cursor = self.tree.walk();
        let point = Document::point_to_pos(pos);

        let mut child = cursor.goto_first_child_for_point(point);

        while let Some(c) = cursor.goto_first_child_for_point(point) {
            child = Some(c);
        }

        child.map(|_| cursor.node())
    }

    pub fn get_item_at_pos(&self, pos: Position) -> Option<QueryableItem> {
        let mut cursor = self.tree.walk();
        let point = Document::point_to_pos(pos);

        let mut child = cursor.goto_first_child_for_point(point);

        while let Some(c) = cursor.goto_first_child_for_point(point) {
            child = Some(c);
        }

        if let Some(_) = child {
            let node = cursor.node();

            let text = match node.utf8_text(self.source.as_bytes()) {
                Ok(s) => s,
                _ => return None,
            };

            let current_node_kind = node.kind();
            cursor.goto_parent();
            let parent_node_kind = cursor.node().kind();

            info!(
                "parent node kind: {}, node kind: {}",
                parent_node_kind, current_node_kind
            );

            if parent_node_kind == NODE_KIND_FN_DEF {
                info!("found function definition; function name: {}", text);
                Some(QueryableItem::Function(String::from(text)))
            } else {
                // we assume, that any other context, in which
                // an identifier occurs, must be an expression
                // and we can check for constants.
                // if this assumption is wrong, we need a way to check,
                // if we are inside an expression

                if current_node_kind == "identifier" {
                    Some(QueryableItem::Constant(String::from(text)))
                } else {
                    None
                }
            }
        } else {
            None
        }
    }

    pub fn point_to_pos(pos: Position) -> Point {
        Point {
            row: pos.line as usize,
            column: pos.character as usize,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DocType {
    Script,
    ScenarioTxt,
    DefCore,
    ActMap,
    Particle,
}

const F_EXT_SCRIPT: &str = "c";
const F_EXT_TXT: &str = "txt";
const F_NAME_SCENARIO: &str = "Scenario.txt";
const F_NAME_DEFCORE: &str = "DefCore.txt";
const F_NAME_ACTMAP: &str = "ActMap.txt";
const F_NAME_PARTICLE: &str = "Particle.txt";

impl DocType {
    pub fn from_uri(uri: &Url) -> anyhow::Result<Self> {
        let file_name = uri
            .path_segments()
            .context("Url cannot be a base")?
            .last()
            .context("Could not extract file name")?;

        let ext = file_name
            .split('.')
            .last()
            .context("Could not get file extension")?;

        match ext {
            F_EXT_SCRIPT => Ok(DocType::Script),
            F_EXT_TXT => match file_name {
                F_NAME_SCENARIO => Ok(DocType::ScenarioTxt),
                F_NAME_DEFCORE => Ok(DocType::DefCore),
                F_NAME_ACTMAP => Ok(DocType::ActMap),
                F_NAME_PARTICLE => Ok(DocType::Particle),
                _ => Err(anyhow!(
                    "File extension '.{}' was recognized, but file name is unknown: {}",
                    &ext,
                    file_name
                )),
            },
            _ => Err(anyhow!("Unrecognized file extension: {}", &ext)),
        }
    }

    pub fn get_handler(&self) -> Box<dyn AssetHandler> {
        match self {
            DocType::Script => Box::new(ScriptHandler::default()),
            DocType::ScenarioTxt => Box::new(ScenarioTxtHandler::default()),
            DocType::DefCore => Box::new(DefCoreHandler::default()),
            DocType::ActMap => Box::new(ActMapHandler::default()),
            DocType::Particle => Box::new(ParticleHandler::default()),
        }
    }

    pub fn get_language(&self) -> Language {
        match self {
            DocType::Script => tree_sitter_c4script::language(),
            DocType::ScenarioTxt => tree_sitter_c4ini::language(),
            DocType::DefCore => tree_sitter_c4ini::language(),
            DocType::ActMap => tree_sitter_c4ini::language(),
            DocType::Particle => tree_sitter_c4ini::language(),
        }
    }

    pub fn get_parser(&self) -> Result<tree_sitter::Parser, tree_sitter::LanguageError> {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(self.get_language())?;

        Ok(parser)
    }
}
