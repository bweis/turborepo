import { Pool } from "pg";

const pool = new Pool();

export async function query(sql: string) {
  return pool.query(sql);
}
