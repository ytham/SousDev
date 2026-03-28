//! Linear issue fetching via the Linear API.
//!
//! Uses `curl` to query the Linear GraphQL API.  Requires a `LINEAR_API_KEY`
//! environment variable.  The response is mapped to [`LinearIssue`] structs
//! that mirror the shape needed by the executor.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// A Linear issue returned by the Linear GraphQL API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearIssue {
    /// Linear's human-readable identifier (e.g. `"ENG-42"`).
    pub identifier: String,
    /// Numeric portion of the identifier, used for dedup and branch naming.
    pub number: u64,
    /// Issue title.
    pub title: String,
    /// Issue description (markdown).
    pub description: Option<String>,
    /// Full Linear URL.
    pub url: String,
    /// Team key (e.g. `"ENG"`).
    pub team_key: String,
    /// State name (e.g. `"Todo"`, `"In Progress"`).
    pub state: String,
    /// Label names.
    pub labels: Vec<String>,
    /// Assignee display name.
    pub assignee: Option<String>,
}

impl LinearIssue {
    /// Returns the description text, or an empty string if `None`.
    pub fn description_str(&self) -> &str {
        self.description.as_deref().unwrap_or("")
    }
}

/// Options for [`fetch_linear_issues`].
pub struct FetchLinearIssuesOptions {
    /// Linear team key (e.g. `"ENG"`).  Required.
    pub team: String,
    /// Only return issues in these states (e.g. `["Todo", "Backlog"]`).
    /// If empty, defaults to `["Todo"]`.
    pub states: Option<Vec<String>>,
    /// Only return issues with ALL of these labels.
    pub labels: Option<Vec<String>>,
    /// Only return issues assigned to this user (display name or email).
    pub assignee: Option<String>,
    /// Maximum number of issues to return (default 10).
    pub limit: Option<usize>,
}

/// Fetch issues from Linear via the GraphQL API.
///
/// Requires the `LINEAR_API_KEY` environment variable to be set.
pub async fn fetch_linear_issues(options: &FetchLinearIssuesOptions) -> Result<Vec<LinearIssue>> {
    let api_key = std::env::var("LINEAR_API_KEY")
        .map_err(|_| anyhow::anyhow!("LINEAR_API_KEY environment variable not set"))?;

    let limit = options.limit.unwrap_or(10);
    let states = options
        .states
        .as_deref()
        .unwrap_or(&[]);
    let default_states = vec!["Todo".to_string()];
    let state_filter = if states.is_empty() {
        &default_states
    } else {
        states
    };

    // Build the GraphQL filter.
    let mut filters = vec![format!(
        "team: {{ key: {{ eq: \"{}\" }} }}",
        options.team
    )];

    // State filter.
    let state_values: Vec<String> = state_filter
        .iter()
        .map(|s| format!("\"{}\"", s))
        .collect();
    filters.push(format!(
        "state: {{ name: {{ in: [{}] }} }}",
        state_values.join(", ")
    ));

    // Label filter.
    if let Some(ref labels) = options.labels {
        if !labels.is_empty() {
            for label in labels {
                filters.push(format!(
                    "labels: {{ name: {{ eq: \"{}\" }} }}",
                    label
                ));
            }
        }
    }

    // Assignee filter.
    if let Some(ref assignee) = options.assignee {
        filters.push(format!(
            "assignee: {{ displayName: {{ eq: \"{}\" }} }}",
            assignee
        ));
    }

    let filter_str = filters.join(", ");

    let query = format!(
        r#"{{
  issues(filter: {{ {} }}, first: {}) {{
    nodes {{
      identifier
      number
      title
      description
      url
      team {{
        key
      }}
      state {{
        name
      }}
      labels {{
        nodes {{
          name
        }}
      }}
      assignee {{
        displayName
      }}
    }}
  }}
}}"#,
        filter_str, limit
    );

    let output = Command::new("curl")
        .arg("-s")
        .arg("-X")
        .arg("POST")
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-H")
        .arg(format!("Authorization: {}", api_key))
        .arg("-d")
        .arg(serde_json::to_string(&serde_json::json!({ "query": query }))?)
        .arg("https://api.linear.app/graphql")
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Linear API request failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let response: LinearGraphQLResponse = serde_json::from_str(&stdout)
        .map_err(|e| anyhow::anyhow!("Failed to parse Linear API response: {} — body: {}", e, stdout))?;

    if let Some(errors) = response.errors {
        let msgs: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
        return Err(anyhow::anyhow!("Linear API errors: {}", msgs.join("; ")));
    }

    let nodes = response
        .data
        .map(|d| d.issues.nodes)
        .unwrap_or_default();

    let issues: Vec<LinearIssue> = nodes
        .into_iter()
        .map(|n| LinearIssue {
            identifier: n.identifier,
            number: n.number,
            title: n.title,
            description: n.description,
            url: n.url,
            team_key: n.team.key,
            state: n.state.name,
            labels: n.labels.nodes.into_iter().map(|l| l.name).collect(),
            assignee: n.assignee.map(|a| a.display_name),
        })
        .collect();

    Ok(issues)
}

// ── GraphQL response types (internal) ─────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LinearGraphQLResponse {
    data: Option<LinearGraphQLData>,
    errors: Option<Vec<LinearGraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct LinearGraphQLData {
    issues: LinearIssuesConnection,
}

#[derive(Debug, Deserialize)]
struct LinearIssuesConnection {
    nodes: Vec<LinearIssueNode>,
}

#[derive(Debug, Deserialize)]
struct LinearIssueNode {
    identifier: String,
    number: u64,
    title: String,
    description: Option<String>,
    url: String,
    team: LinearTeamRef,
    state: LinearStateRef,
    labels: LinearLabelsConnection,
    assignee: Option<LinearAssigneeRef>,
}

#[derive(Debug, Deserialize)]
struct LinearTeamRef {
    key: String,
}

#[derive(Debug, Deserialize)]
struct LinearStateRef {
    name: String,
}

#[derive(Debug, Deserialize)]
struct LinearLabelsConnection {
    nodes: Vec<LinearLabelRef>,
}

#[derive(Debug, Deserialize)]
struct LinearLabelRef {
    name: String,
}

#[derive(Debug, Deserialize)]
struct LinearAssigneeRef {
    #[serde(rename = "displayName")]
    display_name: String,
}

#[derive(Debug, Deserialize)]
struct LinearGraphQLError {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_issue_description_str_some() {
        let issue = LinearIssue {
            identifier: "ENG-42".into(),
            number: 42,
            title: "Fix bug".into(),
            description: Some("The thing is broken".into()),
            url: "https://linear.app/team/ENG-42".into(),
            team_key: "ENG".into(),
            state: "Todo".into(),
            labels: vec![],
            assignee: None,
        };
        assert_eq!(issue.description_str(), "The thing is broken");
    }

    #[test]
    fn test_linear_issue_description_str_none() {
        let issue = LinearIssue {
            identifier: "ENG-1".into(),
            number: 1,
            title: "t".into(),
            description: None,
            url: "u".into(),
            team_key: "ENG".into(),
            state: "Todo".into(),
            labels: vec![],
            assignee: None,
        };
        assert_eq!(issue.description_str(), "");
    }

    #[test]
    fn test_parse_graphql_response() {
        let json = r#"{
            "data": {
                "issues": {
                    "nodes": [
                        {
                            "identifier": "ENG-42",
                            "number": 42,
                            "title": "Fix login bug",
                            "description": "Users can't log in",
                            "url": "https://linear.app/my-team/issue/ENG-42",
                            "team": { "key": "ENG" },
                            "state": { "name": "Todo" },
                            "labels": { "nodes": [{ "name": "bug" }, { "name": "urgent" }] },
                            "assignee": { "displayName": "Alice" }
                        }
                    ]
                }
            }
        }"#;

        let response: LinearGraphQLResponse = serde_json::from_str(json).unwrap();
        let data = response.data.unwrap();
        let nodes = data.issues.nodes;
        assert_eq!(nodes.len(), 1);

        let node = &nodes[0];
        assert_eq!(node.identifier, "ENG-42");
        assert_eq!(node.number, 42);
        assert_eq!(node.title, "Fix login bug");
        assert_eq!(node.team.key, "ENG");
        assert_eq!(node.state.name, "Todo");
        assert_eq!(node.labels.nodes.len(), 2);
        assert_eq!(node.assignee.as_ref().unwrap().display_name, "Alice");
    }

    #[test]
    fn test_parse_graphql_response_with_errors() {
        let json = r#"{
            "errors": [
                { "message": "Authentication required" }
            ]
        }"#;

        let response: LinearGraphQLResponse = serde_json::from_str(json).unwrap();
        assert!(response.data.is_none());
        assert_eq!(response.errors.unwrap()[0].message, "Authentication required");
    }

    #[test]
    fn test_parse_graphql_response_empty_nodes() {
        let json = r#"{
            "data": {
                "issues": {
                    "nodes": []
                }
            }
        }"#;

        let response: LinearGraphQLResponse = serde_json::from_str(json).unwrap();
        let data = response.data.unwrap();
        assert!(data.issues.nodes.is_empty());
    }

    #[test]
    fn test_parse_graphql_response_null_assignee() {
        let json = r#"{
            "data": {
                "issues": {
                    "nodes": [
                        {
                            "identifier": "ENG-1",
                            "number": 1,
                            "title": "Unassigned task",
                            "description": null,
                            "url": "https://linear.app/t/ENG-1",
                            "team": { "key": "ENG" },
                            "state": { "name": "Backlog" },
                            "labels": { "nodes": [] },
                            "assignee": null
                        }
                    ]
                }
            }
        }"#;

        let response: LinearGraphQLResponse = serde_json::from_str(json).unwrap();
        let node = &response.data.unwrap().issues.nodes[0];
        assert!(node.assignee.is_none());
        assert!(node.description.is_none());
    }

    #[test]
    fn test_fetch_options_defaults() {
        let opts = FetchLinearIssuesOptions {
            team: "ENG".into(),
            states: None,
            labels: None,
            assignee: None,
            limit: None,
        };
        assert_eq!(opts.limit.unwrap_or(10), 10);
        assert!(opts.states.is_none());
        assert!(opts.labels.is_none());
        assert!(opts.assignee.is_none());
    }
}
