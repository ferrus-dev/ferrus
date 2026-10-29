//! Consultation and human-answer task transitions.

use super::*;

pub async fn record_task_consultation_requested(
    task_id: &str,
    paused_status: TaskStatus,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        task_consultation_in_transaction(&transaction, task_id, paused_status)?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

pub(crate) fn task_consultation_in_transaction(
    transaction: &Transaction<'_>,
    task_id: String,
    paused_status: TaskStatus,
) -> Result<()> {
    transaction.execute(
        "UPDATE tasks SET status = ?1, paused_status = ?2 WHERE id = ?3",
        params![
            TaskStatus::Consultation.as_str(),
            paused_status.as_str(),
            task_id
        ],
    )?;

    insert_event(
        transaction,
        None,
        "task_consultation_requested",
        &serde_json::json!({
            "task_id": task_id,
            "paused_status": paused_status.as_str(),
        }),
    )?;

    Ok(())
}

pub async fn restore_task_from_consultation(task_id: &str) -> Result<TaskConsultRestore> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<TaskConsultRestore> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = task_restore_consultation_in_transaction(&transaction, task_id)?;
        transaction.commit()?;
        Ok(result)
    })
    .await?
}

pub(crate) fn task_restore_consultation_in_transaction(
    transaction: &Transaction<'_>,
    task_id: String,
) -> Result<TaskConsultRestore> {
    let row = transaction
        .query_row(
            "SELECT status, paused_status FROM tasks WHERE id = ?1",
            [&task_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let Some((status, paused_status)) = row else {
        return Ok(TaskConsultRestore::NotInConsultation);
    };
    if status != TaskStatus::Consultation.as_str() {
        return Ok(TaskConsultRestore::NotInConsultation);
    }

    let resumed_status =
        paused_status.unwrap_or_else(|| TaskStatus::Executing.as_str().to_string());
    transaction.execute(
        "UPDATE tasks SET status = ?1, paused_status = NULL WHERE id = ?2",
        params![resumed_status, task_id],
    )?;
    insert_event_in_transaction(
        transaction,
        None,
        "task_consultation_resolved",
        &serde_json::json!({
            "task_id": task_id,
            "resumed_status": resumed_status,
        }),
    )?;
    Ok(TaskConsultRestore::Restored {
        status: resumed_status,
    })
}

#[cfg(test)]
pub async fn record_task_human_question_requested(
    task_id: &str,
    paused_status: TaskStatus,
    awaiting_human_by: &str,
) -> Result<()> {
    record_task_human_question_requested_with_resume(
        task_id,
        paused_status,
        Some(paused_status),
        awaiting_human_by,
    )
    .await
}

pub async fn record_task_human_question_requested_with_resume(
    task_id: &str,
    resume_status: TaskStatus,
    paused_status: Option<TaskStatus>,
    awaiting_human_by: &str,
) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    let awaiting_human_by = awaiting_human_by.to_string();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        task_human_question_in_transaction(
            &transaction,
            task_id,
            resume_status,
            paused_status,
            awaiting_human_by,
        )?;

        transaction.commit()?;

        Ok(())
    })
    .await?
}

pub(crate) fn task_human_question_in_transaction(
    transaction: &Transaction<'_>,
    task_id: String,
    resume_status: TaskStatus,
    paused_status: Option<TaskStatus>,
    awaiting_human_by: String,
) -> Result<()> {
    let paused_status = paused_status.map(TaskStatus::as_str);
    transaction.execute(
        r#"
        UPDATE tasks
        SET status = ?1, paused_status = ?2, awaiting_human_by = ?3,
            awaiting_human_status = ?4, human_answer_recorded = 0
        WHERE id = ?5
        "#,
        params![
            TaskStatus::AwaitingHuman.as_str(),
            paused_status,
            awaiting_human_by,
            resume_status.as_str(),
            task_id
        ],
    )?;

    insert_event_in_transaction(
        transaction,
        None,
        "task_human_question_requested",
        &serde_json::json!({
            "task_id": task_id,
            "paused_status": paused_status,
            "resume_status": resume_status.as_str(),
            "awaiting_human_by": awaiting_human_by,
        }),
    )?;

    let question_order = transaction.last_insert_rowid();
    transaction.execute(
        "UPDATE tasks SET human_question_order = ?1 WHERE id = ?2",
        params![question_order, task_id],
    )?;

    Ok(())
}

pub async fn record_task_human_answer(task_id: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            r#"
            UPDATE tasks
            SET human_answer_recorded = 1
            WHERE id = ?1 AND status = ?2
            "#,
            params![task_id, TaskStatus::AwaitingHuman.as_str()],
        )?;

        if changed == 0 {
            anyhow::bail!("Task {task_id} is not waiting for a human answer.");
        }

        insert_event_in_transaction(
            &transaction,
            None,
            "task_human_answer_recorded",
            &serde_json::json!({ "task_id": task_id }),
        )?;

        transaction.commit()?;

        Ok(())
    })
    .await?
}

pub async fn record_scoped_human_answer(question: &HumanQuestion, response: &str) -> Result<()> {
    record_task_human_answer(&question.task_id).await?;
    if let Err(err) =
        crate::state::store::write_answer_for_run_dir(&question.run_dir, response).await
    {
        if let Err(rollback_err) = clear_task_human_answer_recorded(&question.task_id).await {
            return Err(err).with_context(|| {
                format!(
                    "Failed to roll back recorded human answer for {}: {rollback_err}",
                    question.task_id
                )
            });
        }

        return Err(err);
    }

    Ok(())
}

pub(super) async fn clear_task_human_answer_recorded(task_id: &str) -> Result<()> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            r#"
            UPDATE tasks
            SET human_answer_recorded = 0
            WHERE id = ?1 AND status = ?2
            "#,
            params![task_id, TaskStatus::AwaitingHuman.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    })
    .await?
}

pub async fn task_human_question_owner(task_id: &str) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();

    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        let owner = connection
            .query_row(
                "SELECT awaiting_human_by FROM tasks WHERE id = ?1",
                [&task_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(owner)
    })
    .await?
}

pub async fn task_awaiting_human_status(task_id: &str) -> Result<Option<String>> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();

    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let connection = open_runtime_database(&database_path)?;
        let status = connection
            .query_row(
                "SELECT awaiting_human_status FROM tasks WHERE id = ?1",
                [&task_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(status)
    })
    .await?
}

pub async fn restore_task_from_human_answer(task_id: &str) -> Result<TaskHumanAnswerRestore> {
    let database_path = current_database_path().await?;
    let task_id = task_id.to_string();

    tokio::task::spawn_blocking(move || -> Result<TaskHumanAnswerRestore> {
        let mut connection = open_runtime_database(&database_path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = task_restore_human_in_transaction(&transaction, task_id)?;
        transaction.commit()?;
        Ok(result)
    })
    .await?
}

pub(crate) fn task_restore_human_in_transaction(
    transaction: &Transaction<'_>,
    task_id: String,
) -> Result<TaskHumanAnswerRestore> {
    let row = transaction
        .query_row(
            "SELECT status, paused_status, awaiting_human_status FROM tasks WHERE id = ?1",
            [&task_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;

    let Some((status, paused_status, awaiting_human_status)) = row else {
        return Ok(TaskHumanAnswerRestore::NotAwaitingHuman);
    };

    if status != TaskStatus::AwaitingHuman.as_str() {
        return Ok(TaskHumanAnswerRestore::NotAwaitingHuman);
    }

    let resumed_status = awaiting_human_status
        .or_else(|| paused_status.clone())
        .unwrap_or_else(|| TaskStatus::Executing.as_str().to_string());

    let restored_paused_status = if resumed_status == TaskStatus::Consultation.as_str() {
        paused_status
    } else {
        None
    };

    transaction.execute(
        r#"
        UPDATE tasks
        SET status = ?1, paused_status = ?2, awaiting_human_by = NULL,
            awaiting_human_status = NULL, human_question_order = NULL,
            human_answer_recorded = 0
        WHERE id = ?3
        "#,
        params![resumed_status, restored_paused_status, task_id],
    )?;

    insert_event_in_transaction(
        transaction,
        None,
        "task_human_answered",
        &serde_json::json!({
            "task_id": task_id,
            "resumed_status": resumed_status,
        }),
    )?;

    Ok(TaskHumanAnswerRestore::Restored {
        status: resumed_status,
    })
}
