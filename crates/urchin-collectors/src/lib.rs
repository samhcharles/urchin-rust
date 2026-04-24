/// urchin-collectors: one module per source.
/// Each collector reads from a tool's native output and produces Events.
/// Collectors are passive — they read, they never write to source tools.

pub mod claude;
pub mod copilot;
pub mod gemini;
pub mod shell;
pub mod git;
pub mod agent_bridge;
