#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Task {
    #[prost(string, tag = "1")]
    pub(super) id: String,
    #[prost(string, tag = "2")]
    pub(super) description: String,
    #[prost(message, optional, tag = "3")]
    pub(super) dependencies: Option<Dependencies>,
    #[prost(message, repeated, tag = "5")]
    pub(super) messages: Vec<Message>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Dependencies {
    #[prost(string, tag = "1")]
    pub(super) parent_task_id: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Message {
    #[prost(string, tag = "1")]
    pub(super) id: String,
    #[prost(message, optional, tag = "2")]
    pub(super) user_query: Option<UserQuery>,
    #[prost(message, optional, tag = "3")]
    pub(super) agent_output: Option<AgentOutput>,
    #[prost(message, optional, tag = "4")]
    pub(super) tool_call: Option<ToolCall>,
    #[prost(message, optional, tag = "5")]
    pub(super) tool_call_result: Option<ToolCallResult>,
    #[prost(message, optional, tag = "14")]
    pub(super) timestamp: Option<Timestamp>,
    #[prost(message, optional, tag = "30")]
    pub(super) request_metadata: Option<RequestMetadata>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Timestamp {
    #[prost(int64, tag = "1")]
    pub(super) seconds: i64,
    #[prost(int32, tag = "2")]
    pub(super) nanos: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct UserQuery {
    #[prost(string, tag = "1")]
    pub(super) query: String,
    #[prost(message, optional, tag = "2")]
    pub(super) context: Option<InputContext>,
    #[prost(message, optional, tag = "6")]
    pub(super) origin: Option<UserQueryOrigin>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct UserQueryOrigin {
    #[prost(oneof = "QueryOrigin", tags = "3, 4, 5, 6, 8")]
    pub(super) variant: Option<QueryOrigin>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(super) enum QueryOrigin {
    #[prost(bytes, tag = "3")]
    ParentAgent(Vec<u8>),
    #[prost(bytes, tag = "4")]
    AgentMessageWake(Vec<u8>),
    #[prost(bytes, tag = "5")]
    Schedule(Vec<u8>),
    #[prost(bytes, tag = "6")]
    Automation(Vec<u8>),
    #[prost(bytes, tag = "8")]
    ServerSynthesized(Vec<u8>),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct AgentOutput {
    #[prost(string, tag = "1")]
    pub(super) text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct InputContext {
    #[prost(message, optional, tag = "1")]
    pub(super) directory: Option<Directory>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Directory {
    #[prost(string, tag = "1")]
    pub(super) pwd: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ToolCall {
    #[prost(string, tag = "1")]
    pub(super) tool_call_id: String,
    #[prost(message, optional, tag = "2")]
    pub(super) run_shell_command: Option<RunShellCommand>,
    #[prost(message, optional, tag = "5")]
    pub(super) read_files: Option<ReadFiles>,
    #[prost(message, optional, tag = "6")]
    pub(super) apply_file_diffs: Option<ApplyFileDiffs>,
    #[prost(
        oneof = "OtherTool",
        tags = "3, 4, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 30, 31, 32, 34, 35, 36, 37, 38"
    )]
    pub(super) other: Option<OtherTool>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(super) enum OtherTool {
    #[prost(bytes, tag = "3")]
    SearchCodebase(Vec<u8>),
    #[prost(bytes, tag = "4")]
    Server(Vec<u8>),
    #[prost(bytes, tag = "7")]
    SuggestPlan(Vec<u8>),
    #[prost(bytes, tag = "8")]
    SuggestCreatePlan(Vec<u8>),
    #[prost(bytes, tag = "9")]
    Grep(Vec<u8>),
    #[prost(bytes, tag = "10")]
    FileGlob(Vec<u8>),
    #[prost(bytes, tag = "11")]
    ReadMcpResource(Vec<u8>),
    #[prost(bytes, tag = "12")]
    CallMcpTool(Vec<u8>),
    #[prost(bytes, tag = "13")]
    WriteToLongRunningShellCommand(Vec<u8>),
    #[prost(bytes, tag = "14")]
    SuggestNewConversation(Vec<u8>),
    #[prost(bytes, tag = "15")]
    FileGlobV2(Vec<u8>),
    #[prost(bytes, tag = "16")]
    SuggestPrompt(Vec<u8>),
    #[prost(bytes, tag = "17")]
    OpenCodeReview(Vec<u8>),
    #[prost(bytes, tag = "18")]
    InitProject(Vec<u8>),
    #[prost(bytes, tag = "19")]
    Subagent(Vec<u8>),
    #[prost(bytes, tag = "20")]
    ReadDocuments(Vec<u8>),
    #[prost(bytes, tag = "21")]
    EditDocuments(Vec<u8>),
    #[prost(bytes, tag = "22")]
    CreateDocuments(Vec<u8>),
    #[prost(bytes, tag = "23")]
    ReadShellCommandOutput(Vec<u8>),
    #[prost(bytes, tag = "24")]
    UseComputer(Vec<u8>),
    #[prost(bytes, tag = "25")]
    InsertReviewComments(Vec<u8>),
    #[prost(bytes, tag = "26")]
    ReadSkill(Vec<u8>),
    #[prost(bytes, tag = "27")]
    RequestComputerUse(Vec<u8>),
    #[prost(bytes, tag = "28")]
    FetchConversation(Vec<u8>),
    #[prost(bytes, tag = "30")]
    SendMessageToAgent(Vec<u8>),
    #[prost(bytes, tag = "31")]
    TransferShellCommandControlToUser(Vec<u8>),
    #[prost(bytes, tag = "32")]
    AskUserQuestion(Vec<u8>),
    #[prost(bytes, tag = "34")]
    UploadFileArtifact(Vec<u8>),
    #[prost(bytes, tag = "35")]
    RunAgents(Vec<u8>),
    #[prost(bytes, tag = "36")]
    WaitForEvents(Vec<u8>),
    #[prost(bytes, tag = "37")]
    StartRecording(Vec<u8>),
    #[prost(bytes, tag = "38")]
    StopRecording(Vec<u8>),
}

impl OtherTool {
    pub(super) fn name(&self) -> &str {
        match self {
            Self::SearchCodebase(_) => "search_codebase",
            Self::Server(_) => "server",
            Self::SuggestPlan(_) => "suggest_plan",
            Self::SuggestCreatePlan(_) => "suggest_create_plan",
            Self::Grep(_) => "grep",
            Self::FileGlob(_) => "file_glob",
            Self::ReadMcpResource(_) => "read_mcp_resource",
            Self::CallMcpTool(_) => "call_mcp_tool",
            Self::WriteToLongRunningShellCommand(_) => "write_to_long_running_shell_command",
            Self::SuggestNewConversation(_) => "suggest_new_conversation",
            Self::FileGlobV2(_) => "file_glob_v2",
            Self::SuggestPrompt(_) => "suggest_prompt",
            Self::OpenCodeReview(_) => "open_code_review",
            Self::InitProject(_) => "init_project",
            Self::Subagent(_) => "subagent",
            Self::ReadDocuments(_) => "read_documents",
            Self::EditDocuments(_) => "edit_documents",
            Self::CreateDocuments(_) => "create_documents",
            Self::ReadShellCommandOutput(_) => "read_shell_command_output",
            Self::UseComputer(_) => "use_computer",
            Self::InsertReviewComments(_) => "insert_review_comments",
            Self::ReadSkill(_) => "read_skill",
            Self::RequestComputerUse(_) => "request_computer_use",
            Self::FetchConversation(_) => "fetch_conversation",
            Self::SendMessageToAgent(_) => "send_message_to_agent",
            Self::TransferShellCommandControlToUser(_) => "transfer_shell_command_control_to_user",
            Self::AskUserQuestion(_) => "ask_user_question",
            Self::UploadFileArtifact(_) => "upload_file_artifact",
            Self::RunAgents(_) => "run_agents",
            Self::WaitForEvents(_) => "wait_for_events",
            Self::StartRecording(_) => "start_recording",
            Self::StopRecording(_) => "stop_recording",
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct RunShellCommand {
    #[prost(string, tag = "1")]
    pub(super) command: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ReadFiles {
    #[prost(message, repeated, tag = "1")]
    pub(super) files: Vec<FileName>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct FileName {
    #[prost(string, tag = "1")]
    pub(super) name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ApplyFileDiffs {
    #[prost(string, tag = "1")]
    pub(super) summary: String,
    #[prost(message, repeated, tag = "2")]
    pub(super) diffs: Vec<FilePath>,
    #[prost(message, repeated, tag = "3")]
    pub(super) new_files: Vec<FilePath>,
    #[prost(message, repeated, tag = "4")]
    pub(super) deleted_files: Vec<FilePath>,
    #[prost(message, repeated, tag = "5")]
    pub(super) v4a_updates: Vec<FileUpdate>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct FilePath {
    #[prost(string, tag = "1")]
    pub(super) file_path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct FileUpdate {
    #[prost(string, tag = "1")]
    pub(super) file_path: String,
    #[prost(string, tag = "2")]
    pub(super) move_to: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ToolCallResult {
    #[prost(string, tag = "1")]
    pub(super) tool_call_id: String,
    #[prost(message, optional, tag = "2")]
    pub(super) run_shell_command: Option<ShellResult>,
    #[prost(message, optional, tag = "5")]
    pub(super) read_files: Option<Outcome>,
    #[prost(message, optional, tag = "6")]
    pub(super) apply_file_diffs: Option<Outcome>,
    #[prost(message, optional, tag = "11")]
    pub(super) context: Option<InputContext>,
    #[prost(bytes = "vec", optional, tag = "14")]
    pub(super) cancel: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Outcome {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub(super) success: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub(super) error: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub(super) any_files_success: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ShellResult {
    #[prost(string, tag = "1")]
    pub(super) output: String,
    #[prost(int32, optional, tag = "2")]
    pub(super) exit_code: Option<i32>,
    #[prost(string, tag = "3")]
    pub(super) command: String,
    #[prost(message, optional, tag = "5")]
    pub(super) command_finished: Option<ShellFinished>,
    #[prost(bytes = "vec", optional, tag = "6")]
    pub(super) permission_denied: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub(super) terminal_busy: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ShellFinished {
    #[prost(string, tag = "1")]
    pub(super) output: String,
    #[prost(int32, tag = "2")]
    pub(super) exit_code: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct RequestMetadata {
    #[prost(message, optional, tag = "2")]
    pub(super) charges: Option<RequestCharges>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct RequestCharges {
    #[prost(btree_map = "string, message", tag = "1")]
    pub(super) usage_by_category: std::collections::BTreeMap<String, ChargedUsage>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ChargedUsage {
    #[prost(btree_map = "string, message", tag = "1")]
    pub(super) direct_api_inference_usage: std::collections::BTreeMap<String, InferenceUsage>,
    #[prost(btree_map = "string, message", tag = "2")]
    pub(super) byok_inference_usage: std::collections::BTreeMap<String, InferenceUsage>,
    #[prost(btree_map = "string, message", tag = "3")]
    pub(super) custom_endpoint_inference_usage: std::collections::BTreeMap<String, InferenceUsage>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct InferenceUsage {
    #[prost(message, optional, tag = "1")]
    pub(super) token_count: Option<TokenCount>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct TokenCount {
    #[prost(uint64, tag = "1")]
    pub(super) input: u64,
    #[prost(uint64, tag = "2")]
    pub(super) output: u64,
    #[prost(uint64, tag = "3")]
    pub(super) input_cache_read: u64,
    #[prost(uint64, tag = "4")]
    pub(super) input_cache_write: u64,
}
