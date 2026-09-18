use std::path::PathBuf;

use anyhow::Result;

use super::{Tool, ToolContext, ToolOutput};

/// Jupyter notebook（.ipynb）单元格编辑工具，对应 Claude Code 的
/// NotebookEdit：按 cell id 或序号替换 / 插入 / 删除单元格，
/// 保留 notebook 的其余字段与输出内容。
pub struct NotebookEdit;

/// 把多行文本编码成 ipynb 的 source 数组：除最后一行外每行带换行符。
fn encode_source(text: &str) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .enumerate()
        .map(|(index, line)| {
            if index + 1 == text.split('\n').count() {
                line.to_string()
            } else {
                format!("{line}\n")
            }
        })
        .collect()
}

#[cfg(test)]
fn decode_source(source: &serde_json::Value) -> String {
    match source {
        serde_json::Value::Array(lines) => lines
            .iter()
            .filter_map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join(""),
        serde_json::Value::String(text) => text.clone(),
        _ => String::new(),
    }
}

#[async_trait::async_trait]
impl Tool for NotebookEdit {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn description(&self) -> &str {
        "Edit a cell in a Jupyter notebook (.ipynb). Identify the cell by its id \
         (preferred) or by its zero-based cell_number. edit_mode: replace (default), \
         insert (new cell before the given index; use a cell_number one past the end \
         to append), or delete. cell_type applies to insert (code|markdown, default code)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "Absolute or workspace-relative path to the .ipynb file"
                },
                "cell_id": {
                    "type": "string",
                    "description": "Cell id to edit (preferred over cell_number)"
                },
                "cell_number": {
                    "type": "integer",
                    "description": "Zero-based cell index used when cell_id is absent"
                },
                "new_source": {
                    "type": "string",
                    "description": "New cell source (ignored for delete)"
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "Cell type for insert (default code)"
                },
                "edit_mode": {
                    "type": "string",
                    "enum": ["replace", "insert", "delete"],
                    "description": "Edit mode (default replace)"
                }
            },
            "required": ["notebook_path", "new_source"]
        })
    }

    fn target_paths(&self, input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
        input
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .map(|path| ctx.resolve_path(path))
            .into_iter()
            .collect()
    }

    async fn call(&self, input: serde_json::Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let Some(path_str) = input.get("notebook_path").and_then(|v| v.as_str()) else {
            return Ok(ToolOutput::err("missing required parameter: notebook_path"));
        };
        let new_source = input
            .get("new_source")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let path = ctx.resolve_path(path_str);
        if path.extension().and_then(|e| e.to_str()) != Some("ipynb") {
            return Ok(ToolOutput::err(format!("{path_str} is not a .ipynb file")));
        }

        let raw = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => raw,
            Err(error) => return Ok(ToolOutput::err(format!("cannot read {path_str}: {error}"))),
        };
        let mut notebook: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => {
                return Ok(ToolOutput::err(format!(
                    "{path_str} is not valid JSON: {error}"
                )))
            }
        };
        let Some(cells) = notebook.get_mut("cells").and_then(|v| v.as_array_mut()) else {
            return Ok(ToolOutput::err(format!("{path_str} has no cells array")));
        };

        let edit_mode = input
            .get("edit_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("replace")
            .to_string();

        // insert 模式先定位插入下标，再构造新单元格。
        if edit_mode == "insert" {
            let index = match locate_cell(cells, &input, path_str, true) {
                Ok(index) => index,
                Err(error) => return Ok(error),
            };
            let cell_type = input
                .get("cell_type")
                .and_then(|v| v.as_str())
                .unwrap_or("code")
                .to_string();
            if cell_type != "code" && cell_type != "markdown" {
                return Ok(ToolOutput::err(format!(
                    "invalid cell_type: {cell_type} (expected code or markdown)"
                )));
            }
            let mut cell = serde_json::json!({
                "cell_type": cell_type,
                "metadata": {},
                "source": encode_source(new_source),
            });
            if cell_type == "code" {
                cell["outputs"] = serde_json::json!([]);
                cell["execution_count"] = serde_json::Value::Null;
            }
            cells.insert(index, cell);
            let total = cells.len();
            write_notebook(&path, &notebook).await?;
            return Ok(ToolOutput::ok(format!(
                "Inserted new {} cell at index {index} in {path_str} ({total})",
                cell_type
            )));
        }

        let index = match locate_cell(cells, &input, path_str, false) {
            Ok(index) => index,
            Err(error) => return Ok(error),
        };

        if edit_mode == "delete" {
            cells.remove(index);
            let total = cells.len();
            write_notebook(&path, &notebook).await?;
            return Ok(ToolOutput::ok(format!(
                "Deleted cell {index} from {path_str} ({total})"
            )));
        }

        if edit_mode != "replace" {
            return Ok(ToolOutput::err(format!(
                "invalid edit_mode: {edit_mode} (expected replace, insert or delete)"
            )));
        }

        let cell = &mut cells[index];
        if let Some(cell_type) = input.get("cell_type").and_then(|v| v.as_str()) {
            if cell_type != "code" && cell_type != "markdown" {
                return Ok(ToolOutput::err(format!(
                    "invalid cell_type: {cell_type} (expected code or markdown)"
                )));
            }
            let previous = cell["cell_type"].as_str().unwrap_or("code");
            if previous != cell_type {
                cell["cell_type"] = serde_json::json!(cell_type);
                match cell_type {
                    "code" => {
                        cell["outputs"] = serde_json::json!([]);
                        cell["execution_count"] = serde_json::Value::Null;
                    }
                    _ => {
                        if let Some(object) = cell.as_object_mut() {
                            object.remove("outputs");
                            object.remove("execution_count");
                        }
                    }
                }
            }
        }
        cell["source"] = serde_json::json!(encode_source(new_source));
        let final_type = cell["cell_type"].as_str().unwrap_or("code").to_string();
        write_notebook(&path, &notebook).await?;
        Ok(ToolOutput::ok(format!(
            "Replaced {} cell {index} in {path_str}",
            final_type
        )))
    }
}

/// 按 cell_id / cell_number 定位单元格下标；insert 模式允许 one-past-end。
fn locate_cell(
    cells: &[serde_json::Value],
    input: &serde_json::Value,
    path: &str,
    inserting: bool,
) -> Result<usize, ToolOutput> {
    let bound = if inserting {
        cells.len()
    } else {
        cells.len().saturating_sub(1)
    };
    if let Some(id) = input.get("cell_id").and_then(|v| v.as_str()) {
        let index = cells
            .iter()
            .position(|cell| cell.get("id").and_then(|v| v.as_str()) == Some(id));
        return index.ok_or_else(|| ToolOutput::err(format!("no cell with id '{id}' in {path}")));
    }
    if let Some(number) = input.get("cell_number").and_then(|v| v.as_u64()) {
        let index = number as usize;
        if index > bound {
            return Err(ToolOutput::err(format!(
                "cell_number {index} is out of range (notebook has {} cells)",
                cells.len()
            )));
        }
        if !inserting && index >= cells.len() {
            return Err(ToolOutput::err(format!(
                "cell_number {index} is out of range (notebook has {} cells)",
                cells.len()
            )));
        }
        return Ok(index);
    }
    Err(ToolOutput::err("either cell_id or cell_number is required"))
}

async fn write_notebook(path: &std::path::Path, notebook: &serde_json::Value) -> Result<()> {
    let pretty = serde_json::to_string_pretty(notebook)?;
    tokio::fs::write(path, pretty).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            read_state: crate::tools::ReadFileState::new(),
            output_dir: dir.join("out"),
            session_id: "test".to_string(),
            todos: Vec::new(),
            background: super::super::background::BackgroundTaskRegistry::new(),
            mode: crate::permissions::PermissionMode::Default,
            pre_plan_mode: None,
        }
    }

    fn sample_notebook() -> String {
        serde_json::json!({
            "cells": [
                {"cell_type": "code", "execution_count": null, "id": "cell-a", "metadata": {},
                 "outputs": [], "source": ["print(\"a\")\n", "print(\"b\")"]},
                {"cell_type": "markdown", "id": "cell-b", "metadata": {}, "source": ["# Title"]}
            ],
            "metadata": {"kernelspec": {"name": "python3"}},
            "nbformat": 4,
            "nbformat_minor": 5
        })
        .to_string()
    }

    fn read_cells(path: &Path) -> Vec<serde_json::Value> {
        let raw = std::fs::read_to_string(path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        value["cells"].as_array().unwrap().clone()
    }

    #[tokio::test]
    async fn replaces_cell_by_id_and_keeps_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        std::fs::write(&path, sample_notebook()).unwrap();
        let mut ctx = make_ctx(dir.path());

        let out = NotebookEdit
            .call(
                serde_json::json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_id": "cell-a",
                    "new_source": "print(\"replaced\")"
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);

        let raw = std::fs::read_to_string(&path).unwrap();
        let notebook: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // 其余字段保留。
        assert_eq!(notebook["metadata"]["kernelspec"]["name"], "python3");
        assert_eq!(notebook["nbformat"], 4);
        let cells = notebook["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(decode_source(&cells[0]["source"]), "print(\"replaced\")");
        assert_eq!(cells[0]["outputs"].as_array().unwrap().len(), 0);
        // markdown 单元格未受影响。
        assert_eq!(decode_source(&cells[1]["source"]), "# Title");
    }

    #[tokio::test]
    async fn insert_and_delete_by_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        std::fs::write(&path, sample_notebook()).unwrap();
        let mut ctx = make_ctx(dir.path());

        let out = NotebookEdit
            .call(
                serde_json::json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_number": 1,
                    "edit_mode": "insert",
                    "cell_type": "markdown",
                    "new_source": "## Inserted"
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let cells = read_cells(&path);
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[1]["cell_type"], "markdown");
        assert_eq!(decode_source(&cells[1]["source"]), "## Inserted");
        assert!(
            cells[1].get("outputs").is_none(),
            "markdown cell has no outputs"
        );

        let out = NotebookEdit
            .call(
                serde_json::json!({
                    "notebook_path": path.to_string_lossy(),
                    "cell_number": 0,
                    "edit_mode": "delete",
                    "new_source": ""
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let cells = read_cells(&path);
        assert_eq!(cells.len(), 2);
        // 删除的是 cell_number 0（原 cell-a）：剩下插入的 markdown 与 cell-b。
        assert_eq!(decode_source(&cells[0]["source"]), "## Inserted");
        assert_eq!(cells[1]["id"], "cell-b");
    }

    #[tokio::test]
    async fn errors_on_bad_target_and_args() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nb.ipynb");
        std::fs::write(&path, sample_notebook()).unwrap();
        let mut ctx = make_ctx(dir.path());

        // 未知 cell id。
        let out = NotebookEdit
            .call(
                serde_json::json!({"notebook_path": path.to_string_lossy(), "cell_id": "nope", "new_source": "x"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);

        // 越界 cell_number。
        let out = NotebookEdit
            .call(
                serde_json::json!({"notebook_path": path.to_string_lossy(), "cell_number": 9, "new_source": "x"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);

        // 缺少定位参数。
        let out = NotebookEdit
            .call(
                serde_json::json!({"notebook_path": path.to_string_lossy(), "new_source": "x"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);

        // 非 .ipynb 文件。
        let out = NotebookEdit
            .call(
                serde_json::json!({"notebook_path": "README.md", "cell_number": 0, "new_source": "x"}),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[test]
    fn source_encoding_round_trip() {
        assert_eq!(
            encode_source("a\nb"),
            vec!["a\n".to_string(), "b".to_string()]
        );
        assert_eq!(encode_source(""), Vec::<String>::new());
        let encoded = encode_source("a\nb");
        assert_eq!(decode_source(&serde_json::json!(encoded)), "a\nb");
    }
}
