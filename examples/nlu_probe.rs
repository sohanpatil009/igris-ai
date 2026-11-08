// Quick diagnostic for the offline NLU path.
//
// Run with:
//   cargo run --example nlu_probe
//
// It prints the detected intent and confidence for a set of realistic
// commands. The expected intent is shown so regressions are obvious.

use igris_ai::nlu::engine::NluEngine;

fn main() -> anyhow::Result<()> {
    let mut engine = NluEngine::new();
    engine.initialize()?;

    let cases: &[(&str, &str)] = &[
        ("open chrome", "open_app"),
        ("launch firefox", "open_app"),
        ("start notepad", "open_app"),
        ("open the downloads folder", "file_operation"),
        ("close chrome", "close_app"),
        ("quit spotify", "close_app"),
        ("shut down chrome", "close_app"),
        ("shut down the computer", "system_control"),
        ("restart my pc", "system_control"),
        ("lock the screen", "system_control"),
        ("turn up the volume", "system_control"),
        ("mute the sound", "system_control"),
        ("turn off wifi", "system_control"),
        ("increase brightness", "system_control"),
        ("search for rust tutorials", "web_search"),
        ("what is the weather in london", "web_search"),
        ("take a screenshot", "camera_control"),
        ("take a photo", "camera_control"),
        ("start recording video", "camera_control"),
        ("remind me to call mom in 10 minutes", "set_reminder"),
        ("set an alarm for 7 am", "set_alarm"),
        ("read the file notes.txt", "file_operation"),
        ("create a file called todo.md", "file_operation"),
        ("who are you", "about_igris"),
        ("good morning", "greeting"),
        ("stop listening", "assistant_control"),
    ];

    println!("{:<42} {:<20} {:>6}  expected", "command", "intent", "conf");
    for (command, expected) in cases {
        let result = engine.process_input(command)?;
        let mark = if result.intent_name == *expected {
            "ok"
        } else {
            "MISS"
        };
        println!(
            "{:<42} {:<20} {:>6.3}  {} ({})",
            command, result.intent_name, result.confidence, expected, mark
        );
    }
    Ok(())
}
