use pgmq_core::util::{conn_options, fetch_one_message};
use rand::Rng;
use sqlx::postgres::PgPoolOptions;
use sqlx::{FromRow, Pool, Postgres, Row};

async fn connect(url: &str) -> Pool<Postgres> {
    let options = conn_options(url).expect("failed to parse url");
    println!("URL: {}", url);
    PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(10))
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("failed to connect to pg")
}

#[tokio::test]
async fn test_lifecycle() {
    let username = whoami::username();

    let conn00 = connect(&format!(
        "postgres://{username}:postgres@localhost:28815/pgmq"
    ))
    .await;
    // ignore the error if the db already exists!
    let _ = sqlx::query("CREATE DATABASE pgmq_test;")
        .execute(&conn00)
        .await;
    conn00.close().await;

    let conn = connect(&format!(
        "postgres://{username}:postgres@localhost:28815/pgmq_test"
    ))
    .await;
    let mut rng = rand::thread_rng();
    let test_num = rng.gen_range(0..100000);

    // DROP EXTENSION
    // requires pg_partman to already be installed in the instance
    let _ = sqlx::query("DROP EXTENSION IF EXISTS pgmq")
        .execute(&conn)
        .await
        .expect("failed to drop extension");

    // CREATE EXTENSION
    let _ = sqlx::query("CREATE EXTENSION pgmq cascade")
        .execute(&conn)
        .await
        .expect("failed to create extension");

    // CREATE with default retention and partition strategy
    let test_default_queue = format!("test_default_{test_num}");
    let _ = sqlx::query(&format!("SELECT pgmq_create('{test_default_queue}');"))
        .execute(&conn)
        .await
        .expect("failed to create queue");

    // creating a queue must be idempotent
    // create with same name again, must be no  error
    let _ = sqlx::query(&format!("SELECT pgmq_create('{test_default_queue}');"))
        .execute(&conn)
        .await
        .expect("failed to create queue");

    let msg_id = sqlx::query(&format!(
        "SELECT * from pgmq_send('{test_default_queue}', '{{\"hello\": \"world\"}}');"
    ))
    .fetch_one(&conn)
    .await
    .expect("failed send")
    .get::<i64, usize>(0);
    assert_eq!(msg_id, 1);

    // read message
    // vt=2, limit=1
    let query = &format!("SELECT * from pgmq_read('{test_default_queue}', 2, 1);");

    let message = fetch_one_message::<serde_json::Value>(query, &conn)
        .await
        .expect("failed reading message")
        .expect("expected message");
    assert_eq!(message.msg_id, 1);

    // set VT to in 10 seconds
    let query = &format!("SELECT * from pgmq_set_vt('{test_default_queue}', {msg_id}, 5);");
    let message = fetch_one_message::<serde_json::Value>(query, &conn)
        .await
        .expect("failed reading message")
        .expect("expected message");
    assert_eq!(message.msg_id, 1);
    let now = chrono::offset::Utc::now();
    // closish to 10 seconds from now
    assert!(message.vt > now + chrono::Duration::seconds(4));

    // read again, assert no messages because we just set VT to the future
    let query = &format!("SELECT * from pgmq_read('{test_default_queue}', 2, 1);");
    let message = fetch_one_message::<serde_json::Value>(query, &conn)
        .await
        .expect("failed reading message");
    assert!(message.is_none());

    // read again, now using poll to block until message is ready
    let query = &format!("SELECT * from pgmq_read_with_poll('{test_default_queue}', 10, 1, 10);");
    let message = fetch_one_message::<serde_json::Value>(query, &conn)
        .await
        .expect("failed reading message")
        .expect("expected message");
    assert_eq!(message.msg_id, 1);

    // after reading it, set VT to now
    let query = &format!("SELECT * from pgmq_set_vt('{test_default_queue}', {msg_id}, 0);");
    let message = fetch_one_message::<serde_json::Value>(query, &conn)
        .await
        .expect("failed reading message")
        .expect("expected message");
    assert_eq!(message.msg_id, 1);

    // read again, should have msg_id 1 again
    let query = &format!("SELECT * from pgmq_read('{test_default_queue}', 2, 1);");
    let message = fetch_one_message::<serde_json::Value>(query, &conn)
        .await
        .expect("failed reading message")
        .expect("expected message");
    assert_eq!(message.msg_id, 1);

    // send a batch of 2 messages
    let batch_queue = format!("test_batch_{test_num}");
    let _ = sqlx::query(&format!("SELECT pgmq_create('{batch_queue}');"))
        .execute(&conn)
        .await
        .expect("failed to create queue");
    let msg_ids = sqlx::query(
        &format!("select pgmq_send_batch('{batch_queue}', ARRAY['{{\"hello\": \"world_0\"}}'::jsonb, '{{\"hello\": \"world_1\"}}'::jsonb])")
    )
    .fetch_all(&conn).await.expect("failed to send batch");
    assert_eq!(msg_ids.len(), 2);
    assert_eq!(msg_ids[0].get::<i64, usize>(0), 1);
    assert_eq!(msg_ids[1].get::<i64, usize>(0), 2);
    let rowcount: i64 = sqlx::query_scalar(&format!("SELECT count(*) from pgmq_{batch_queue}"))
        .fetch_one(&conn)
        .await
        .expect("failed to get rowcount");
    assert_eq!(rowcount, 2);

    let _ = sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_partman")
        .execute(&conn)
        .await
        .expect("failed to create extension");

    // CREATE with 5 seconds per partition, 10 seconds retention
    let test_duration_queue = format!("test_duration_{test_num}");
    let q = format!("SELECT \"pgmq_create_partitioned\"('{test_duration_queue}'::text, '5 seconds'::text, '10 seconds'::text);");
    let _ = sqlx::query(&q)
        .execute(&conn)
        .await
        .expect("failed creating duration queue");

    // CREATE with 10 messages per partition, 20 messages retention
    let test_numeric_queue = format!("test_numeric_{test_num}");
    let _ = sqlx::query(&format!(
        "SELECT \"pgmq_create_partitioned\"('{test_numeric_queue}'::text, '10'::text, '20'::text);"
    ))
    .execute(&conn)
    .await
    .expect("failed creating numeric interval queue");

    #[allow(dead_code)]
    #[derive(FromRow)]
    struct MetricsRow {
        queue_name: String,
        queue_length: i64,
        newest_msg_age_sec: Option<i32>,
        oldest_msg_age_sec: Option<i32>,
        total_messages: i64,
        scrape_time: chrono::DateTime<chrono::Utc>,
    }

    // get metrics
    let rows = sqlx::query_as::<_, MetricsRow>(&format!(
        "SELECT * from pgmq_metrics('{test_duration_queue}'::text);"
    ))
    .fetch_all(&conn)
    .await
    .expect("failed creating numeric interval queue");
    assert_eq!(rows.len(), 1);

    // get metrics all
    let rows = sqlx::query_as::<_, MetricsRow>(&format!("SELECT * from pgmq_metrics_all();"))
        .fetch_all(&conn)
        .await
        .expect("failed creating numeric interval queue");
    assert!(rows.len() > 1);

    // delete all the queues
    #[allow(dead_code)]
    #[derive(FromRow)]
    struct QueueMeta {
        queue_name: String,
    }

    // delete partitioned queues
    for queue in [test_duration_queue, test_numeric_queue].iter() {
        sqlx::query(&format!("select pgmq_drop_queue('{}', true);", &queue))
            .execute(&conn)
            .await
            .expect("failed to drop partitioned queues");
    }

    let queues = sqlx::query_as::<_, QueueMeta>("select queue_name from pgmq_list_queues();")
        .fetch_all(&conn)
        .await
        .expect("failed to list queues");

    // drop the rest of the queues
    for queue in queues {
        let q = queue.queue_name;
        sqlx::query(&format!("select pgmq_drop_queue('{}');", &q))
            .execute(&conn)
            .await
            .expect("failed to drop standard queues");
    }

    let queues = sqlx::query_as::<_, QueueMeta>("select queue_name from pgmq_list_queues();")
        .fetch_all(&conn)
        .await
        .expect("failed to list queues");
    assert!(queues.is_empty());

    #[allow(dead_code)]
    #[derive(FromRow)]
    struct ResultSet {
        num_partmans: i64,
    }
    let partmans =
        sqlx::query_as::<_, ResultSet>("select count(*) as num_partmans from part_config;")
            .fetch_one(&conn)
            .await
            .expect("failed to query partman config");
    assert_eq!(partmans.num_partmans, 0);
}
