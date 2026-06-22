use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from("/tmp");
    let agent_id = "";  // Empty agent_id
    let chat_id = 12345i64;
    
    let path = root
        .join("agents")
        .join(agent_id)
        .join("channels/telegram")
        .join(format!("{}.md", chat_id));
    
    println!("Path with empty agent_id: {:?}", path);
    println!("Parent: {:?}", path.parent());
    println!("Parent is Some: {}", path.parent().is_some());
}
