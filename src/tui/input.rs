//! Editor and key event translation; no networking or application state mutation.
#[derive(Debug, Default)]
pub struct Editor {
    pub text: String,
    pub revision: u64,
}
impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn clear(&mut self) {
        self.text.clear();
        self.revision += 1
    }
}
