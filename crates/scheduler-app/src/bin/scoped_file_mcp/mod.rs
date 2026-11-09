use std::path::Path;

use anyhow::Result;
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, JsonObject,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};
use scheduler_runtime::scoped_files::FileBroker;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PathInput {
    /// A project-relative path using forward slashes. Use "." to list the project root.
    path: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    /// A project-relative file path allowed by the task's write permissions.
    path: String,
    /// The complete UTF-8 file content to write to the task workspace.
    text: String,
}

struct ScopedFiles {
    broker: FileBroker,
    tools: Vec<Tool>,
}

impl ScopedFiles {
    fn execute(&self, request: CallToolRequestParams) -> Result<Value> {
        let input = Value::Object(request.arguments.unwrap_or_default());
        match request.name.as_ref() {
            "read_file" => {
                let PathInput { path } = serde_json::from_value(input)?;
                Ok(json!({ "text": self.broker.read_file(&path)? }))
            }
            "list_directory" => {
                let PathInput { path } = serde_json::from_value(input)?;
                Ok(json!({ "entries": self.broker.list_directory(&path)? }))
            }
            "write_file" => {
                let WriteInput { path, text } = serde_json::from_value(input)?;
                self.broker.write_file(&path, &text)?;
                Ok(json!({ "written": path }))
            }
            _ => anyhow::bail!("Unknown scoped file tool"),
        }
    }
}

impl ServerHandler for ScopedFiles {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("runspur-scoped-files", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        Ok(match self.execute(request) {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::structured_error(json!({ "message": error.to_string() })),
        }
        .into())
    }
}

pub async fn serve(ticket: &Path) -> Result<()> {
    let files = ScopedFiles {
        broker: FileBroker::from_ticket(ticket)?,
        tools: vec![
            Tool::new("read_file", "Read a UTF-8 project file, including changes in the task workspace.", JsonObject::new())
                .with_input_schema::<PathInput>(),
            Tool::new("list_directory", "List up to 256 entries in a project directory, including new files in the task workspace when project-wide writes are enabled.", JsonObject::new())
                .with_input_schema::<PathInput>(),
            Tool::new("write_file", "Write a complete UTF-8 file in the task workspace. Use an exact path authorized by the task, or a project-relative path when project-wide writes are enabled.", JsonObject::new())
                .with_input_schema::<WriteInput>(),
        ],
    };
    let service = files.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
