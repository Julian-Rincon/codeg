//! Adds per-turn tool-quality counters to `token_usage_turn`, for the model
//! scorecard (`commands::model_scorecard`).
//!
//! Twelve columns: an overall `tool_calls`/`tool_errors` pair, plus the same
//! pair for each of the five categories the scorecard ranks models on (edit,
//! read, shell, web, agent). A tool whose normalized name matches none of the
//! five categories still counts toward the overall pair — see
//! `commands::token_usage::categorize_tool`.
//!
//! Every column is `NOT NULL DEFAULT 0`, matching the token-counter columns
//! this table already carries (see `m20260803_000001_token_usage`) — a row
//! written before this migration reads as "zero tool calls recorded" rather
//! than NULL, which is the correct answer for a fact whose transcript simply
//! predates this accounting.
//!
//! No backfill migration for EXISTING rows: getting real counts into them
//! means re-parsing every conversation's transcript, which only the sync pass
//! can do (this migration only touches schema). That re-sync is triggered by
//! bumping `FACT_SCHEMA_VERSION` in `commands::token_usage` to `"4"` — the
//! established mechanism for "previously written rows are incomplete, not
//! merely stale" (see that constant's doc comment). The next
//! `token_usage_sync` (any mode) then rebuilds every conversation with the new
//! counters populated, exactly as it already does for accounting fixes.
//!
//! SQLite allows one `ADD COLUMN` per `ALTER TABLE` statement, so this is
//! twelve separate statements (same pattern as `m20260801_000002_work_task_p2`).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const COLUMNS: [TokenUsageTurn; 12] = [
    TokenUsageTurn::ToolCalls,
    TokenUsageTurn::ToolErrors,
    TokenUsageTurn::EditCalls,
    TokenUsageTurn::EditErrors,
    TokenUsageTurn::ReadCalls,
    TokenUsageTurn::ReadErrors,
    TokenUsageTurn::ShellCalls,
    TokenUsageTurn::ShellErrors,
    TokenUsageTurn::WebCalls,
    TokenUsageTurn::WebErrors,
    TokenUsageTurn::AgentCalls,
    TokenUsageTurn::AgentErrors,
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for col in COLUMNS {
            manager
                .alter_table(
                    Table::alter()
                        .table(TokenUsageTurn::Table)
                        .add_column(ColumnDef::new(col).big_integer().not_null().default(0))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Reverse order, matching the sibling multi-column migrations.
        for col in COLUMNS.into_iter().rev() {
            manager
                .alter_table(
                    Table::alter()
                        .table(TokenUsageTurn::Table)
                        .drop_column(col)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[derive(DeriveIden, Clone, Copy)]
enum TokenUsageTurn {
    Table,
    ToolCalls,
    ToolErrors,
    EditCalls,
    EditErrors,
    ReadCalls,
    ReadErrors,
    ShellCalls,
    ShellErrors,
    WebCalls,
    WebErrors,
    AgentCalls,
    AgentErrors,
}
