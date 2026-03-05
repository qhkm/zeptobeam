use zeptoclaw::tools::Tool;

/// Builds tool sets for ZeptoAgent instances.
/// Implementations know about available tools and filter
/// by an optional whitelist.
pub trait ToolFactory: Send + Sync {
    fn build_tools(&self, whitelist: Option<&[String]>) -> Vec<Box<dyn Tool>>;
}
