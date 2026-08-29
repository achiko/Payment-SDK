use deadpool_postgres::Client;
use indexing::IndexScope;

/// Deletes only one benchmark scope, with children removed before parents.
pub(crate) async fn clear_scope(
    client: &mut Client,
    scope: &IndexScope,
) -> Result<(), tokio_postgres::Error> {
    const DELETE_SCOPE: [&str; 6] = [
        "DELETE FROM movement WHERE chain = $1 AND network = $2",
        "DELETE FROM history WHERE chain = $1 AND network = $2",
        "DELETE FROM journal_output WHERE chain = $1 AND network = $2",
        "DELETE FROM journal WHERE chain = $1 AND network = $2",
        "DELETE FROM output WHERE chain = $1 AND network = $2",
        "DELETE FROM checkpoint WHERE chain = $1 AND network = $2",
    ];

    let transaction = client.transaction().await?;
    for statement in DELETE_SCOPE {
        transaction
            .execute(statement, &[&scope.chain.0, &scope.network])
            .await?;
    }
    transaction.commit().await
}
