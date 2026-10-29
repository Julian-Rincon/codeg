use sea_orm::entity::prelude::*;

/// One working-tree snapshot Phantom took for a conversation: before each
/// prompt (`kind = "prompt"`) or before a restore overwrote the tree
/// (`kind = "safety"`). `commit_sha` is `None` when the snapshot was skipped
/// (timeout, git failure) — the row still marks the gap. Written and read only
/// by `crate::phantom::checkpoints`. See `m20261028_000001_phantom_checkpoint`.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "phantom_checkpoint")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub conversation_id: i32,
    /// Per-conversation counter; also the last segment of `ref_name`.
    pub seq: i32,
    /// `prompt` or `safety`.
    #[sea_orm(column_type = "Text")]
    pub kind: String,
    /// The user message id the prompt went out under, when the sender named
    /// one (the UI's optimistic turn id).
    #[sea_orm(column_type = "Text", nullable)]
    pub message_id: Option<String>,
    /// The prompt's text, trimmed — matches a transcript turn to its row.
    #[sea_orm(column_type = "Text", nullable)]
    pub prompt_preview: Option<String>,
    /// `git rev-parse --show-toplevel` of the working dir at snapshot time.
    #[sea_orm(column_type = "Text")]
    pub repo_root: String,
    #[sea_orm(column_type = "Text", nullable)]
    pub ref_name: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub commit_sha: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub tree_sha: Option<String>,
    /// Files that differ from the previous checkpoint (or from HEAD for the
    /// first one).
    pub changed_files: i32,
    /// The tree equalled the previous checkpoint's, so its commit was reused.
    pub deduped: bool,
    #[sea_orm(column_type = "Text", nullable)]
    pub skipped_reason: Option<String>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id",
        on_delete = "Cascade"
    )]
    Conversation,
}

impl Related<super::conversation::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Conversation.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
