//! Tenant scopes: rows of a model with a `#[model(tenant)]` column are only
//! visible through a [`Db`](crate::Db) or [`Transaction`](crate::Transaction)
//! that names the tenant, or that explicitly opts out of the scope.

use crate::error::{Error, Result};
use crate::model::Model;
use crate::sql::Cond;
use crate::value::Value;

/// Which tenant's rows an executor may see.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum Scope {
    /// None chosen: tenant-scoped models refuse to run.
    #[default]
    Unset,
    /// Only rows whose tenant column equals this value.
    Tenant(Value),
    /// Every tenant's rows, chosen explicitly.
    All,
}

impl Scope {
    /// The tenant filter for a table whose tenant column is `column`.
    pub(crate) fn filter(&self, column: Option<&'static str>, table: &str) -> Result<Option<Cond>> {
        match (column, self) {
            (None, _) | (_, Scope::All) => Ok(None),
            (Some(column), Scope::Tenant(tenant)) => Ok(Some(Cond::Compare(column, "=", tenant.clone()))),
            (Some(_), Scope::Unset) => Err(Error::TenantScope(format!(
                "`{}` is tenant-scoped: use db.for_tenant(..), or db.all_tenants() / query.all_tenants() to see every tenant",
                table
            ))),
        }
    }

    /// The tenant filter for model `M`.
    pub(crate) fn filter_for<M: Model>(&self) -> Result<Option<Cond>> {
        self.filter(M::TENANT, M::TABLE)
    }

    /// Checks that a row about to be written belongs to the scope's tenant.
    pub(crate) fn check_row<M: Model>(&self, model: &M) -> Result<()> {
        let (Some(column), Scope::Tenant(tenant)) = (M::TENANT, self) else {
            return self.filter_for::<M>().map(|_| ());
        };
        match model.get(column) {
            Some(value) if value.clone().key() == tenant.clone().key() => Ok(()),
            _ => Err(Error::WrongTenant(format!("this `{}` row belongs to another tenant", M::TABLE))),
        }
    }

    /// `conds` plus the tenant filter for `M`, with subqueries on
    /// tenant-scoped tables (from `has` and `where_has`) filtered too.
    pub(crate) fn apply<M: Model>(&self, conds: &[Cond]) -> Result<Vec<Cond>> {
        let mut scoped = self.apply_to(conds)?;
        scoped.extend(self.filter_for::<M>()?);
        Ok(scoped)
    }

    /// `conds` with subqueries on tenant-scoped tables filtered.
    pub(crate) fn apply_to(&self, conds: &[Cond]) -> Result<Vec<Cond>> {
        let mut scoped = conds.to_vec();
        for cond in &mut scoped {
            self.scope_subqueries(cond)?;
        }
        Ok(scoped)
    }

    fn scope_subqueries(&self, cond: &mut Cond) -> Result<()> {
        match cond {
            Cond::And(conds) | Cond::Or(conds) => conds.iter_mut().try_for_each(|c| self.scope_subqueries(c)),
            Cond::Not(cond) => self.scope_subqueries(cond),
            Cond::InSubquery(_, sub, _) => {
                for inner in &mut sub.conds {
                    self.scope_subqueries(inner)?;
                }
                if let Some(filter) = self.filter(sub.tenant, sub.table)? {
                    sub.conds.push(filter);
                    sub.tenant = None; // Applied.
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
