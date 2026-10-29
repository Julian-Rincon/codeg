use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // phantom_checkpoint (Phantom-owned): one row per prompt a conversation
        // sent from a git working tree, naming the snapshot commit Phantom
        // stored under `refs/phantom/checkpoints/<conversation_id>/<seq>` just
        // before the prompt reached the agent — what "Rebobinar aquí" restores.
        // `kind = "safety"` rows are the snapshots a restore takes of the tree
        // it is about to overwrite, so the restore can be undone.
        //
        // A prompt whose snapshot could not be taken still gets a row, with no
        // commit and the reason in `skipped_reason`: matching a message to "the
        // checkpoint before it" must find that gap rather than silently fall
        // back to an older, wrong tree. See `crate::phantom::checkpoints`.
        manager
            .create_table(
                Table::create()
                    .table(PhantomCheckpoint::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(PhantomCheckpoint::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(PhantomCheckpoint::ConversationId)
                            .integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(PhantomCheckpoint::Seq).integer().not_null())
                    .col(ColumnDef::new(PhantomCheckpoint::Kind).text().not_null())
                    .col(ColumnDef::new(PhantomCheckpoint::MessageId).text().null())
                    .col(
                        ColumnDef::new(PhantomCheckpoint::PromptPreview)
                            .text()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(PhantomCheckpoint::RepoRoot)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(PhantomCheckpoint::RefName).text().null())
                    .col(ColumnDef::new(PhantomCheckpoint::CommitSha).text().null())
                    .col(ColumnDef::new(PhantomCheckpoint::TreeSha).text().null())
                    .col(
                        ColumnDef::new(PhantomCheckpoint::ChangedFiles)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(PhantomCheckpoint::Deduped)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(PhantomCheckpoint::SkippedReason)
                            .text()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(PhantomCheckpoint::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_phantom_checkpoint_conversation_id")
                            .from(PhantomCheckpoint::Table, PhantomCheckpoint::ConversationId)
                            .to(Conversation::Table, Conversation::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // A conversation's checkpoints in order: the next `seq`, the dialog's
        // lookup and the prune all read this.
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_phantom_checkpoint_conversation_seq")
                    .table(PhantomCheckpoint::Table)
                    .col(PhantomCheckpoint::ConversationId)
                    .col(PhantomCheckpoint::Seq)
                    .unique()
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PhantomCheckpoint::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum PhantomCheckpoint {
    Table,
    Id,
    ConversationId,
    Seq,
    Kind,
    MessageId,
    PromptPreview,
    RepoRoot,
    RefName,
    CommitSha,
    TreeSha,
    ChangedFiles,
    Deduped,
    SkippedReason,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Conversation {
    Table,
    Id,
}
