use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let message = if args.is_empty() {
        "Hello, async world!".to_string()
    } else {
        args.join(" ")
    };

    println!("Starting...");
    sleep(Duration::from_millis(100)).await;
    println!("{}", message);
    println!("Done!");
}
