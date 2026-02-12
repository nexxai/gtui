use sqlx::Row;
use sqlx::sqlite::SqlitePoolOptions;
use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <search_query>", args[0]);
        eprintln!("Search query matches against Sender or Subject.");
        std::process::exit(1);
    }

    let query = &args[1];
    let search_term = format!("%{}%", query);

    let database_url = "sqlite://gtui.db";
    let pool = SqlitePoolOptions::new()
        .connect(database_url)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to database: {}", e))?;

    let row = sqlx::query(
        "SELECT id, from_address, subject, internal_date, body_plain, body_html 
         FROM messages 
         WHERE from_address LIKE ? OR subject LIKE ?
         ORDER BY internal_date DESC 
         LIMIT 1",
    )
    .bind(&search_term)
    .bind(&search_term)
    .fetch_optional(&pool)
    .await?;

    if let Some(row) = row {
        let id: String = row.get("id");
        let from: Option<String> = row.get("from_address");
        let subject: Option<String> = row.get("subject");
        let date: i64 = row.get("internal_date");
        let body_plain: Option<String> = row.get("body_plain");
        let body_html: Option<String> = row.get("body_html");

        println!("Found Message:");
        println!("ID: {}", id);
        println!("From: {:?}", from);
        println!("Subject: {:?}", subject);
        println!("Date: {}", date);
        println!(
            "--------------------------------------------------------------------------------"
        );
        println!("BODY PLAIN (Raw Debug):");
        println!("{:?}", body_plain);
        println!(
            "--------------------------------------------------------------------------------"
        );
        println!("BODY PLAIN (Display):");
        if let Some(ref text) = body_plain {
            println!("{}", text);
        } else {
            println!("(None)");
        }
        println!(
            "--------------------------------------------------------------------------------"
        );
        println!("BODY HTML (Raw Debug):");
        println!("{:?}", body_html);
        println!(
            "--------------------------------------------------------------------------------"
        );
    } else {
        println!("No messages found matching '{}'", query);
    }

    Ok(())
}
