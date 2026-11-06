//! Queue snapshots are committed atomically under the existing single-owner DB.
use super::*;
impl Store {
    pub fn rollup_load(&self, name: &str) -> Result<Option<crate::rollup::Repository>> {
        let value: Option<String> = self
            .db()?
            .query_row(
                "SELECT payload FROM rollup_repositories WHERE name=?1",
                [name.to_ascii_lowercase()],
                |r| r.get(0),
            )
            .optional()?;
        value
            .map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    }
    pub fn rollup_save(&self, repo: &crate::rollup::Repository) -> Result<()> {
        self.db()?.execute("INSERT INTO rollup_repositories(name,payload) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET payload=excluded.payload", params![repo.name.to_ascii_lowercase(), serde_json::to_string(repo)?])?;
        Ok(())
    }
    pub fn rollup_all(&self) -> Result<Vec<crate::rollup::Repository>> {
        let db = self.db()?;
        let mut q = db.prepare("SELECT payload FROM rollup_repositories ORDER BY name")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|v| Ok(serde_json::from_str(&v?)?)).collect()
    }
}
