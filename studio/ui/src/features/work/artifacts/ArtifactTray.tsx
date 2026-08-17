import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { listRunArtifacts, type RunArtifact } from "../../../lib/api/artifacts";
import { bytePreview } from "../../../lib/text";
import styles from "./Artifacts.module.css";
import { ArtifactInspector } from "./ArtifactInspector";

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MiB`;
}

export function ArtifactTray({ runId }: { runId: string }) {
  const [selected, setSelected] = useState<RunArtifact | null>(null);
  const artifacts = useQuery({
    queryKey: ["artifacts", runId],
    queryFn: () => listRunArtifacts({ run_id: runId }),
    enabled: Boolean(runId),
  });

  return (
    <>
      <section className={styles.tray} aria-labelledby="outputs-heading">
        <header>
          <div>
            <span className="eyebrow">Outputs</span>
            <h3 id="outputs-heading">Run-produced artifacts</h3>
          </div>
        </header>
        {artifacts.isLoading ? (
          <p className={styles.empty}>Loading outputs…</p>
        ) : artifacts.isError ? (
          <p className={styles.empty}>Could not load outputs for this run.</p>
        ) : !artifacts.data?.artifacts.length ? (
          <p className={styles.empty}>No artifacts were committed for this run.</p>
        ) : (
          <ul>
            {artifacts.data.artifacts.map((artifact: RunArtifact) => (
              <li key={artifact.artifact_id}>
                <button type="button" onClick={() => setSelected(artifact)} aria-label={`Open ${artifact.name ?? bytePreview(artifact.artifact_id, 24).text}`}>
                  <span className={styles.kind}>{artifact.media_kind}</span>
                  <span className={styles.name}>{artifact.name ?? bytePreview(artifact.artifact_id, 24).text}</span>
                  <span className={styles.meta}>{formatBytes(Number(artifact.versions[0]?.bytes ?? 0))} · {artifact.media_type ?? "unknown type"}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>
      {selected && <ArtifactInspector artifact={selected} onClose={() => setSelected(null)} />}
    </>
  );
}
