use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expense {
    pub id: Uuid,
    pub description: String,
    pub amount: f64,
    pub category: String,
    pub date: NaiveDate,
}

#[derive(Debug, Deserialize)]
pub struct CreateExpense {
    pub description: String,
    pub amount: f64,
    pub category: String,
    pub date: NaiveDate,
}

#[derive(Debug, Deserialize)]
pub struct UpdateExpense {
    pub description: Option<String>,
    pub amount: Option<f64>,
    pub category: Option<String>,
    pub date: Option<NaiveDate>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ExpenseQuery {
    pub category: Option<String>,
    pub month: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CategorySummary {
    pub category: String,
    pub total: f64,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct SummaryResponse {
    pub month: Option<String>,
    pub total: f64,
    pub by_category: Vec<CategorySummary>,
}

#[derive(Clone)]
pub struct ExpenseStore(pub Arc<RwLock<HashMap<Uuid, Expense>>>);

impl ExpenseStore {
    pub fn seeded() -> Self {
        let mut map = HashMap::new();
        let seed = [
            ("Team lunch", 42.50, "Food", 2026, 7, 2),
            ("Monitor stand", 89.00, "Office", 2026, 7, 5),
            ("Client dinner", 120.00, "Food", 2026, 7, 10),
            ("Domain renewal", 14.99, "Software", 2026, 7, 12),
            ("Taxi to airport", 35.00, "Travel", 2026, 6, 28),
            ("Conference ticket", 350.00, "Travel", 2026, 6, 15),
        ];
        for (description, amount, category, year, month, day) in seed {
            let id = Uuid::new_v4();
            map.insert(
                id,
                Expense {
                    id,
                    description: description.to_string(),
                    amount,
                    category: category.to_string(),
                    date: NaiveDate::from_ymd_opt(year, month, day).expect("valid seed date"),
                },
            );
        }
        Self(Arc::new(RwLock::new(map)))
    }
}

#[derive(Debug)]
pub enum ExpenseError {
    NotFound,
    BadRequest(String),
}

impl IntoResponse for ExpenseError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "expense not found".to_string()),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

pub async fn list(
    State(state): State<crate::api::AppState>,
    Query(query): Query<ExpenseQuery>,
) -> Json<Vec<Expense>> {
    let guard = state.expenses.0.read().await;
    let mut result: Vec<_> = guard
        .values()
        .filter(|expense| matches_category(&expense.category, &query.category))
        .filter(|expense| matches_month(&expense.date, &query.month))
        .cloned()
        .collect();
    result.sort_by_key(|expense| expense.date);
    Json(result)
}

pub async fn get(
    State(state): State<crate::api::AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Expense>, ExpenseError> {
    state
        .expenses
        .0
        .read()
        .await
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(ExpenseError::NotFound)
}

pub async fn create(
    State(state): State<crate::api::AppState>,
    Json(payload): Json<CreateExpense>,
) -> Result<(StatusCode, Json<Expense>), ExpenseError> {
    if payload.amount < 0.0 {
        return Err(ExpenseError::BadRequest("amount must be >= 0".to_string()));
    }
    if payload.description.trim().is_empty() {
        return Err(ExpenseError::BadRequest(
            "description must not be empty".to_string(),
        ));
    }
    let id = Uuid::new_v4();
    let expense = Expense {
        id,
        description: payload.description,
        amount: payload.amount,
        category: payload.category,
        date: payload.date,
    };
    state.expenses.0.write().await.insert(id, expense.clone());
    Ok((StatusCode::CREATED, Json(expense)))
}

pub async fn update(
    State(state): State<crate::api::AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateExpense>,
) -> Result<Json<Expense>, ExpenseError> {
    let mut guard = state.expenses.0.write().await;
    let expense = guard.get_mut(&id).ok_or(ExpenseError::NotFound)?;
    if let Some(description) = payload.description {
        if description.trim().is_empty() {
            return Err(ExpenseError::BadRequest(
                "description must not be empty".to_string(),
            ));
        }
        expense.description = description;
    }
    if let Some(amount) = payload.amount {
        if amount < 0.0 {
            return Err(ExpenseError::BadRequest("amount must be >= 0".to_string()));
        }
        expense.amount = amount;
    }
    if let Some(category) = payload.category {
        expense.category = category;
    }
    if let Some(date) = payload.date {
        expense.date = date;
    }
    Ok(Json(expense.clone()))
}

pub async fn delete(
    State(state): State<crate::api::AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ExpenseError> {
    state
        .expenses
        .0
        .write()
        .await
        .remove(&id)
        .ok_or(ExpenseError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn summary(
    State(state): State<crate::api::AppState>,
    Query(query): Query<ExpenseQuery>,
) -> Json<SummaryResponse> {
    let guard = state.expenses.0.read().await;
    let filtered: Vec<_> = guard
        .values()
        .filter(|expense| matches_month(&expense.date, &query.month))
        .collect();
    let mut totals: HashMap<String, (f64, usize)> = HashMap::new();
    for expense in &filtered {
        let category = totals.entry(expense.category.clone()).or_insert((0.0, 0));
        category.0 += expense.amount;
        category.1 += 1;
    }
    let mut by_category: Vec<_> = totals
        .into_iter()
        .map(|(category, (total, count))| CategorySummary {
            category,
            total,
            count,
        })
        .collect();
    by_category.sort_by(|left, right| {
        right
            .total
            .partial_cmp(&left.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Json(SummaryResponse {
        month: query.month,
        total: filtered.iter().map(|expense| expense.amount).sum(),
        by_category,
    })
}

fn matches_month(date: &NaiveDate, month: &Option<String>) -> bool {
    month
        .as_ref()
        .map(|value| format!("{:04}-{:02}", date.year(), date.month()) == *value)
        .unwrap_or(true)
}

fn matches_category(category: &str, filter: &Option<String>) -> bool {
    filter
        .as_ref()
        .map(|value| category.eq_ignore_ascii_case(value))
        .unwrap_or(true)
}
