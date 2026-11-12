use lbug::{Connection, Database, SystemConfig};
fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("lge_spike.lbug");
    let _ = std::fs::remove_file(&dir); let _ = std::fs::remove_dir_all(&dir);
    let db = Database::new(&dir, SystemConfig::default())?;
    let conn = Connection::new(&db)?;
    println!("lbug {} source={}", lbug::VERSION, lbug::get_library_source());
    conn.query("CREATE NODE TABLE Symbol(id STRING, name STRING, doc STRING, PRIMARY KEY(id));")?;
    conn.query("CREATE REL TABLE CALLS(FROM Symbol TO Symbol, confidence DOUBLE, method STRING);")?;
    conn.query("CREATE (:Symbol {id:'a', name:'create_task', doc:'creates a task'}), (:Symbol {id:'b', name:'save_task', doc:'persists task to db'});")?;
    conn.query("MATCH (a:Symbol {id:'a'}),(b:Symbol {id:'b'}) CREATE (a)-[:CALLS {confidence:0.9, method:'static'}]->(b);")?;
    println!("{}", conn.query("MATCH (a)-[c:CALLS*1..3]->(b) RETURN a.name, b.name;")?);
    for ext in ["fts", "algo", "vector"] {
        let r = conn.query(&format!("INSTALL {ext};")).and_then(|_| conn.query(&format!("LOAD {ext};")));
        println!("ext {ext}: {}", match r { Ok(_) => "ok".into(), Err(e) => format!("ERR {e}") });
    }
    match conn.query("CALL CREATE_FTS_INDEX('Symbol','sym_fts',['name','doc']);") {
        Ok(_) => println!("{}", conn.query("CALL QUERY_FTS_INDEX('Symbol','sym_fts','task') RETURN node.name, score ORDER BY score DESC;")?),
        Err(e) => println!("fts index ERR {e}"),
    }
    Ok(())
}
