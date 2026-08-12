import { useMutation, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import {
  createExperiment,
  createGate,
  listDatasets,
  listExperiments,
  listGates,
  type ExperimentRecord,
  type GateRecord,
} from "../../../lib/api/evaluations";
import { useConnectionStore } from "../../../state/connection";
import styles from "./ExperimentWorkbench.module.css";

export function ExperimentWorkbench() {
  const { connection } = useConnectionStore();
  const datasets = useQuery({
    queryKey: connection ? [connectionScope(connection), "datasets"] : ["datasets", "idle"],
    queryFn: () => (connection ? listDatasets(connection) : Promise.resolve([])),
    enabled: Boolean(connection),
  });
  const experiments = useQuery({
    queryKey: connection ? [connectionScope(connection), "experiments"] : ["experiments", "idle"],
    queryFn: () => (connection ? listExperiments(connection) : Promise.resolve([])),
    enabled: Boolean(connection),
  });
  const gates = useQuery({
    queryKey: connection ? [connectionScope(connection), "gates"] : ["gates", "idle"],
    queryFn: () => (connection ? listGates(connection) : Promise.resolve([])),
    enabled: Boolean(connection),
  });

  const [experimentError, setExperimentError] = useState("");
  const [gateError, setGateError] = useState("");
  const [experimentCandidate, setExperimentCandidate] = useState("");
  const [experimentMetric, setExperimentMetric] = useState("case_pass_rate");
  const [selectedDataset, setSelectedDataset] = useState("");
  const [gateName, setGateName] = useState("");
  const [gateTarget, setGateTarget] = useState("");
  const [gateMetric, setGateMetric] = useState("case_pass_rate");
  const [gateThreshold, setGateThreshold] = useState("0.95");

  const createExperimentMutation = useMutation({
    mutationFn: async () => {
      if (!connection) throw new Error("Not connected");
      const [name, version] = selectedDataset.split("@");
      if (!name || !version) throw new Error("Select a dataset version");
      if (!experimentCandidate.trim()) throw new Error("Candidate id is required");
      return createExperiment(connection, {
        candidate_id: experimentCandidate.trim(),
        dataset_name: name,
        dataset_version: version,
        target_metric: experimentMetric.trim(),
        thresholds: { max_pass_rate_drop: 0.05 },
      });
    },
    onSuccess: () => {
      setExperimentError("");
      experiments.refetch();
    },
    onError: (e) => setExperimentError(e instanceof Error ? e.message : String(e)),
  });

  const createGateMutation = useMutation({
    mutationFn: async () => {
      if (!connection) throw new Error("Not connected");
      if (!gateName.trim() || !gateTarget.trim() || !gateMetric.trim()) {
        throw new Error("Name, blocked target, and metric are required");
      }
      const threshold = parseFloat(gateThreshold);
      if (Number.isNaN(threshold)) throw new Error("Threshold must be a number");
      return createGate(connection, {
        name: gateName.trim(),
        blocked_target: gateTarget.trim(),
        metric: gateMetric.trim(),
        threshold,
        dataset_version: selectedDataset || "*",
      });
    },
    onSuccess: () => {
      setGateError("");
      gates.refetch();
    },
    onError: (e) => setGateError(e instanceof Error ? e.message : String(e)),
  });

  if (!connection) {
    return <p className={styles.hint}>Connect Rusty to manage experiments and gates.</p>;
  }

  return (
    <section className={styles.workbench} aria-labelledby="experiment-workbench-heading">
      <h3 id="experiment-workbench-heading">Evaluation workbench</h3>

      <div className={styles.columns}>
        <div className={styles.column}>
          <h4>Published datasets</h4>
          {datasets.isLoading && <p>Loading datasets…</p>}
          {datasets.data && datasets.data.length === 0 && <p className={styles.hint}>No server datasets yet.</p>}
          {datasets.data && datasets.data.length > 0 && (
            <ul className={styles.recordList}>
              {datasets.data.map((dataset) => (
                <li key={`${dataset.name}@${dataset.version}`}>
                  <button
                    type="button"
                    className={selectedDataset === `${dataset.name}@${dataset.version}` ? styles.selected : ""}
                    onClick={() => setSelectedDataset(`${dataset.name}@${dataset.version}`)}
                  >
                    <code>{dataset.name}@{dataset.version}</code>
                    <span>{dataset.case_count} cases</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className={styles.column}>
          <h4>Experiments</h4>
          <form
            className={styles.miniForm}
            onSubmit={(event) => {
              event.preventDefault();
              createExperimentMutation.mutate();
            }}
          >
            <label>
              Candidate id
              <input value={experimentCandidate} onChange={(event) => setExperimentCandidate(event.target.value)} placeholder="candidate-1" />
            </label>
            <label>
              Target metric
              <input value={experimentMetric} onChange={(event) => setExperimentMetric(event.target.value)} />
            </label>
            <button type="submit" className="primary-button" disabled={!selectedDataset || createExperimentMutation.isPending}>
              {createExperimentMutation.isPending ? "Saving…" : "Save experiment"}
            </button>
            {experimentError && <p className={styles.error} role="alert">{experimentError}</p>}
          </form>
          {experiments.data && experiments.data.length > 0 && (
            <ul className={styles.recordList}>
              {experiments.data.map((experiment: ExperimentRecord) => (
                <li key={experiment.experiment_id}>
                  <code>{experiment.experiment_id}</code>
                  <span>{experiment.status}</span>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className={styles.column}>
          <h4>Gates</h4>
          <form
            className={styles.miniForm}
            onSubmit={(event) => {
              event.preventDefault();
              createGateMutation.mutate();
            }}
          >
            <label>
              Gate name
              <input value={gateName} onChange={(event) => setGateName(event.target.value)} />
            </label>
            <label>
              Blocked target
              <input value={gateTarget} onChange={(event) => setGateTarget(event.target.value)} placeholder="prompt:system@prod" />
            </label>
            <label>
              Metric
              <input value={gateMetric} onChange={(event) => setGateMetric(event.target.value)} />
            </label>
            <label>
              Threshold
              <input value={gateThreshold} onChange={(event) => setGateThreshold(event.target.value)} />
            </label>
            <button type="submit" className="primary-button" disabled={createGateMutation.isPending}>
              {createGateMutation.isPending ? "Saving…" : "Save gate"}
            </button>
            {gateError && <p className={styles.error} role="alert">{gateError}</p>}
          </form>
          {gates.data && gates.data.length > 0 && (
            <ul className={styles.recordList}>
              {gates.data.map((gate: GateRecord) => (
                <li key={gate.name}>
                  <code>{gate.name}</code>
                  <span>{gate.metric} ≥ {gate.threshold}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
    </section>
  );
}

function connectionScope(connection: { epoch: number; origin: string; tenantFingerprint: string }) {
  return `${connection.epoch}:${connection.origin}:${connection.tenantFingerprint}`;
}
