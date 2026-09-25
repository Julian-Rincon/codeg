use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// One usage-bearing turn, materialized for the token dashboard.
///
/// Written only by the sync pass in `commands::token_usage`, which replaces a
/// conversation's whole row set inside one transaction — so a row is never
/// updated in place and never partially refreshed.
///
/// `folder_id` / `agent_type` are intentionally absent: the dashboard reaches
/// them by joining `conversation`, so folder moves, agent changes and soft
/// deletes take effect without a re-sync. See the migration for the rationale.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "token_usage_turn")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub conversation_id: i32,
    /// The parser's `MessageTurn::id` — provenance for debugging, not a key.
    pub turn_key: String,
    /// When the spend happened: the turn's `completed_at` when the parser knows
    /// it, otherwise its `timestamp`.
    pub occurred_at: DateTimeUtc,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    /// Sum of the four counters above.
    pub total_tokens: i64,
    pub duration_ms: i64,
    /// Tool-quality counters, populated from the turn's `ToolUse`/`ToolResult`
    /// content blocks (see `commands::token_usage::tool_counters_from_blocks`).
    /// All `NOT NULL DEFAULT 0` — a pre-existing row reads as "no tool calls
    /// recorded" until the next full sync re-derives it. `tool_calls` /
    /// `tool_errors` count every tool call regardless of category; the five
    /// category pairs are a subset of that total (a tool whose normalized name
    /// matches no category still counts only toward the overall pair).
    pub tool_calls: i64,
    pub tool_errors: i64,
    pub edit_calls: i64,
    pub edit_errors: i64,
    pub read_calls: i64,
    pub read_errors: i64,
    pub shell_calls: i64,
    pub shell_errors: i64,
    pub web_calls: i64,
    pub web_errors: i64,
    pub agent_calls: i64,
    pub agent_errors: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::conversation::Entity",
        from = "Column::ConversationId",
        to = "super::conversation::Column::Id"
    )]
    Conversation,
}

impl Related<super::conversation::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Conversation.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
