use chrono::NaiveDate;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct VisitorCsvRow {
    date: NaiveDate,
    hour: i32,
    camera_name: String,
    visitor_count: i64,
}

pub async fn export_visitor_csv(
    pool: &PgPool,
    store_id: &Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<String, sqlx::Error> {
    let rows = sqlx::query_as::<_, VisitorCsvRow>(
        "SELECT \
             vc.counted_at::date AS date, \
             EXTRACT(HOUR FROM vc.counted_at)::int AS hour, \
             c.name AS camera_name, \
             SUM(vc.people_count)::bigint AS visitor_count \
         FROM visitor_counts vc \
         JOIN cameras c ON c.id = vc.camera_id \
         WHERE vc.store_id = $1 \
           AND vc.counted_at::date >= $2 \
           AND vc.counted_at::date <= $3 \
         GROUP BY vc.counted_at::date, EXTRACT(HOUR FROM vc.counted_at), c.name \
         ORDER BY date, hour, camera_name",
    )
    .bind(store_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    let mut buf = String::from("日付,時間,カメラ名,来客数\n");
    for row in &rows {
        let camera_name = if row.camera_name.contains(',') || row.camera_name.contains('"') {
            format!("\"{}\"", row.camera_name.replace('"', "\"\""))
        } else {
            row.camera_name.clone()
        };
        buf.push_str(&format!(
            "{},{},{},{}\n",
            row.date, row.hour, camera_name, row.visitor_count
        ));
    }

    Ok(buf)
}
