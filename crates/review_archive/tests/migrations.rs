//! Every migration's `down` undoes its `up`: up → down → up leaves the same schema.

use sqlx::{Row, sqlite::SqliteConnectOptions};

async fn schema(pool: &sqlx::SqlitePool) -> Vec<String> {
	sqlx::query("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE '_sqlx%' AND name NOT LIKE 'sqlite_%' ORDER BY name")
		.fetch_all(pool)
		.await
		.unwrap()
		.iter()
		.map(|r| r.get::<String, _>(0))
		.collect()
}

#[tokio::test]
async fn up_down_up_round_trips() {
	let dir = tempfile::tempdir().unwrap();
	let pool = sqlx::SqlitePool::connect_with(SqliteConnectOptions::new().filename(dir.path().join("m.db")).create_if_missing(true))
		.await
		.unwrap();
	let migrator = sqlx::migrate!("./migrations");

	migrator.run(&pool).await.unwrap();
	let first = schema(&pool).await;
	assert!(!first.is_empty());

	migrator.undo(&pool, 0).await.unwrap();
	assert!(schema(&pool).await.is_empty(), "down leaves nothing behind");

	migrator.run(&pool).await.unwrap();
	assert_eq!(schema(&pool).await, first);
}
