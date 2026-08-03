
import { MockDataSource, type DataSource } from "./data/datasource";
import { TauriDataSource } from "./data/tauri-datasource";
import { makeMockClusterGroups } from "./data/mock-data";


function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export const dataSource: DataSource = inTauri()
  ? new TauriDataSource()
  : new MockDataSource(makeMockClusterGroups(120));
