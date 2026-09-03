use std::io::Write;

#[derive(Debug, Clone)]
pub enum UiEvent {
    AssistantText(String),
    Thinking(String),
    ToolStarted {
        name: String,
    },
    Yielded {
        question: String,
        kind: Option<String>,
    },
    Done,
}

pub struct Renderer {
    pub show_thinking: bool,
    in_thinking: bool,
}

impl Renderer {
    pub fn new(show_thinking: bool) -> Self {
        Self {
            show_thinking,
            in_thinking: false,
        }
    }

    pub fn render(&mut self, event: UiEvent) {
        match event {
            UiEvent::AssistantText(t) => {
                if self.in_thinking {
                    println!();
                    self.in_thinking = false;
                }
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            UiEvent::Thinking(t) => {
                if self.show_thinking {
                    if !self.in_thinking {
                        print!("\x1b[2m[thinking] ");
                        self.in_thinking = true;
                    }
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                }
            }
            UiEvent::ToolStarted { name } => {
                if self.in_thinking {
                    println!("\x1b[0m");
                    self.in_thinking = false;
                }
                eprintln!("\x1b[36m[tool] {name}\x1b[0m");
            }
            UiEvent::Yielded { question, kind } => {
                if self.in_thinking {
                    println!("\x1b[0m");
                    self.in_thinking = false;
                }
                println!();
                match kind {
                    Some(k) => println!("\x1b[33m[yield:{k}] {question}\x1b[0m"),
                    None => println!("\x1b[33m[yield] {question}\x1b[0m"),
                }
            }
            UiEvent::Done => {
                if self.in_thinking {
                    print!("\x1b[0m");
                    self.in_thinking = false;
                }
                println!();
            }
        }
    }
}
