import { useMutation, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { createDataset, listDatasets, type EvalCase } from "../../../lib/api/evaluations";
import { useConnectionStore } from "../../../state/connection";
import { evaluationDatasetJsonl, type EvaluationCase } from "../../../state/work";
import styles from "./DatasetPublisher.module.css";

export function DatasetPublisher({ cases }: { cases: EvaluationCase[] }) {
  const { connection } = useConnectionStore();
  const [name, setName] = useState("rusty-studio-evaluations");
  const [version, setVersion] = useState(() => new Date().toISOString().slice(0, 10));
  const [error, setError] = useState("");
  const datasets = useQuery({
    queryKey: connection ? [connectionScope(connection), "datasets"] : ["datasets", "idle"],
    queryFn: () => (connection ? listDatasets(connection) : Promise.resolve([])),
    enabled: Boolean(connection),
  });
  const publish = useMutation({
    mutationFn: async () => {
      if (!connection) throw new Error("Not connected");
      if (!name.trim() || !version.trim()) throw new Error("Name and version are required");
      const evalCases: EvalCase[] = cases.map((item) => ({
        id: item.caseId,
        input: { objective: item.objective },
        expect: { state: [{ pointer: item.pointer, expected: item.expected }] },
        tags: ["studio"],
      }));
      return createDataset(connection, { name: name.trim(), version: version.trim(), cases: evalCases });
    },
    onSuccess: () => {
      setError("");
      datasets.refetch();
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  if (!connection) {
    return <p className={styles.hint}>Connect Rusty to publish server-backed datasets.</p>;
  }
  return (
    <section className={styles.publisher} aria-labelledby="dataset-publisher-heading">
      <h3 id="dataset-publisher-heading">Publish dataset version</h3>
      <p>Seal the current page-memory cases as an immutable server-backed dataset version.</p>
      <div className={styles.fields}>
        <label>
          Dataset name
          <input value={name} onChange={(event) => setName(event.target.value)} />
        </label>
        <label>
          Version
          <input value={version} onChange={(event) => setVersion(event.target.value)} />
        </label>
      </div>
      <button
        type="button"
        className="primary-button"
        disabled={cases.length === 0 || publish.isPending}
        onClick={() => publish.mutate()}
      >
        {publish.isPending ? "Publishing…" : `Publish ${cases.length} case${cases.length === 1 ? "" : "s"}`}
      </button>
      {error && <p className={styles.error} role="alert">{error}</p>}
      {publish.isSuccess && <p className={styles.success}>Published {publish.data.name}@{publish.data.version} with digest {publish.data.digest.slice(0, 12)}.</p>}
      {datasets.data && datasets.data.length > 0 && (
        <div className={styles.list}>
          <span className="eyebrow">Published datasets</span>
          <ul>
            {datasets.data.map((dataset) => (
              <li key={`${dataset.name}@${dataset.version}`}>
                <code>{dataset.name}@{dataset.version}</code>
                <span>{dataset.case_count} cases</span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </section>
  );
}

function connectionScope(connection: { epoch: number; origin: string; tenantFingerprint: string }) {
  return `${connection.epoch}:${connection.origin}:${connection.tenantFingerprint}`;
}
