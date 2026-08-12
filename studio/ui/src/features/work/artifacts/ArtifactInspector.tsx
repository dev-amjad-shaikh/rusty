import { useMutation, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { ArtifactImage } from "./ArtifactImage";
import { getRunArtifactBytes, getRunArtifactNamed, getRunArtifactPreview, listRunArtifactVersions, releaseRunArtifact, type ArtifactPreview, type RunArtifact } from "../../../lib/api/artifacts";
import { useConnectionStore } from "../../../state/connection";
import { bytePreview } from "../../../lib/text";
import styles from "./Artifacts.module.css";

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MiB`;
}

function retentionLabel(policy: RunArtifact["retention"]) {
  if (policy.policy === "pinned") return "Pinned";
  if (policy.policy === "days") return `Kept for ${policy.days} days`;
  return "Receipt protected";
}

function PreviewView({ preview }: { preview: ArtifactPreview }) {
  switch (preview.kind) {
    case "text":
      return (
        <div className={styles.preview}>
          <h4>Text preview</h4>
          <pre className={styles.textPreview}>{preview.text}</pre>
          {preview.truncated && <p className={styles.emptyPreview}>Preview truncated. Download for exact bytes.</p>}
        </div>
      );
    case "json":
      return (
        <div className={styles.preview}>
          <h4>JSON preview</h4>
          <pre className={styles.jsonPreview}>{JSON.stringify(preview.value, null, 2)}</pre>
        </div>
      );
    case "image":
      return (
        <div className={styles.preview}>
          <h4>Image preview · {preview.width}×{preview.height}</h4>
          <ArtifactImage ppmHex={preview.pixels_ppm_hex} alt="Derived thumbnail" />
        </div>
      );
    case "audio": {
      const max = Math.max(1, ...preview.peaks);
      return (
        <div className={styles.preview}>
          <h4>Audio preview</h4>
          <div className={styles.audioPreview}>
            <div className={styles.waveform} aria-hidden="true">
              {preview.peaks.map((peak, index) => <div key={index} className={styles.bar} style={{ height: `${(peak / max) * 100}%` }} />)}
            </div>
            <p className={styles.meta}>{preview.duration_ms} ms · {preview.sample_rate} Hz · {preview.channels} channels</p>
          </div>
        </div>
      );
    }
    case "empty":
      return <div className={styles.preview}><h4>Preview unavailable</h4><p className={styles.emptyPreview}>{preview.reason}</p></div>;
  }
}

async function streamDownload(response: Response, filename: string) {
  const picker = (window as unknown as { showSaveFilePicker?: (options: { suggestedName: string }) => Promise<{ createWritable: () => Promise<{ write: (chunk: Uint8Array) => Promise<void>; close: () => Promise<void> }> }> }).showSaveFilePicker;
  if (picker) {
    const handle = await picker({ suggestedName: filename });
    const writable = await handle.createWritable();
    const reader = response.body?.getReader();
    if (!reader) throw new Error("Download stream unavailable.");
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        await writable.write(value);
      }
    } finally {
      await writable.close();
    }
    return;
  }
  // Fallback for browsers without File System Access API: only for reasonably small artifacts.
  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  anchor.click();
  URL.revokeObjectURL(url);
}

function VersionsPanel({ name }: { name: string }) {
  const { connection } = useConnectionStore();
  const versions = useQuery({
    queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "artifact-versions", name] : ["artifact-versions", "idle"],
    queryFn: () => listRunArtifactVersions(connection!, name),
    enabled: Boolean(connection && name),
  });
  if (versions.isLoading) return <p className={styles.emptyPreview}>Loading version history…</p>;
  if (versions.isError) return <p className={styles.emptyPreview}>Version history could not be loaded.</p>;
  if (!versions.data?.versions.length) return null;
  return (
    <div className={styles.preview}>
      <h4>Version history · {versions.data.versions.length}</h4>
      <ol className={styles.versionList}>
        {versions.data.versions.map((version, index) => (
          <li key={version.sha256}>
            <span>v{index}</span>
            <code>{bytePreview(version.sha256, 24).text}</code>
            <span>{formatBytes(Number(version.bytes))}</span>
            <span>{new Date(version.committed_at).toLocaleDateString()}</span>
          </li>
        ))}
      </ol>
    </div>
  );
}

function ReleasePanel({ artifact, onReleased }: { artifact: RunArtifact; onReleased: () => void }) {
  const { connection } = useConnectionStore();
  const [author, setAuthor] = useState("");
  const [reason, setReason] = useState("");
  const release = useMutation({
    mutationFn: () => {
      if (!connection) throw new Error("Not connected.");
      return releaseRunArtifact(connection, artifact.artifact_id, { released_by: author, reason: reason || undefined });
    },
    onSuccess: () => { setAuthor(""); setReason(""); onReleased(); },
  });
  return (
    <div className={styles.preview}>
      <h4>Release stored bytes</h4>
      <p className={styles.emptyPreview}>This shortens evidence retention. Preview and download may stop while metadata and lineage remain.</p>
      <label>Author<input type="text" value={author} onChange={(event) => setAuthor(event.target.value)} placeholder="operator id" /></label>
      <label>Reason (optional)<input type="text" value={reason} onChange={(event) => setReason(event.target.value)} placeholder="why this release is safe" /></label>
      {release.isError && <p className={styles.emptyPreview} role="alert">{release.error instanceof Error ? release.error.message : "Release failed."}</p>}
      <button type="button" className="secondary-button" onClick={() => release.mutate()} disabled={!author.trim() || release.isPending}>{release.isPending ? "Releasing…" : "Release stored bytes"}</button>
    </div>
  );
}

export function ArtifactInspector({ artifact, onClose }: { artifact: RunArtifact; onClose: () => void }) {
  const { connection } = useConnectionStore();
  const [view, setView] = useState<"preview" | "versions" | "release">("preview");
  const preview = useQuery({
    queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "artifact-preview", artifact.artifact_id] : ["artifact-preview", "idle"],
    queryFn: () => getRunArtifactPreview(connection!, artifact.artifact_id),
    enabled: Boolean(connection),
  });
  const named = useQuery({
    queryKey: connection && artifact.name ? [connection.epoch, connection.origin, connection.tenantFingerprint, "artifact-named", artifact.name] : ["artifact-named", "idle"],
    queryFn: () => getRunArtifactNamed(connection!, artifact.name!),
    enabled: Boolean(connection && artifact.name),
  });
  const download = useMutation({
    mutationFn: async () => {
      if (!connection) throw new Error("Not connected.");
      const response = await getRunArtifactBytes(connection, artifact.artifact_id);
      const filename = artifact.name ? `${artifact.name.replace(/[^A-Za-z0-9._-]/g, "_")}` : `artifact-${artifact.artifact_id.slice(0, 16)}`;
      const extension = artifact.media_type ? `.${artifact.media_type.split("/").pop() ?? "bin"}` : "";
      await streamDownload(response, `${filename}${extension}`);
    },
  });

  const current = named.data ?? artifact;

  return (
    <div className={styles.inspectorBackdrop} role="dialog" aria-modal="true" aria-labelledby="artifact-title" onClick={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div className={styles.inspector}>
        <header>
          <div>
            <span className="eyebrow">Artifact</span>
            <h2 id="artifact-title">{artifact.name ?? bytePreview(artifact.artifact_id, 24).text}</h2>
          </div>
          <button type="button" onClick={onClose}>Close</button>
        </header>
        <div className={styles.body}>
          <dl>
            <div><dt>Identity</dt><dd><code>{bytePreview(current.artifact_id, 48).text}</code></dd></div>
            <div><dt>Media kind</dt><dd>{current.media_kind}</dd></div>
            <div><dt>Declared type</dt><dd>{current.media_type ?? "Not declared"}</dd></div>
            <div><dt>Size</dt><dd>{formatBytes(Number(current.versions[0]?.bytes ?? 0))}</dd></div>
            <div><dt>Retention</dt><dd>{retentionLabel(current.retention)}</dd></div>
            <div><dt>Committed</dt><dd>{new Date(current.created_at).toLocaleString()}</dd></div>
          </dl>
          <div className={styles.lineage}>
            <h4>Lineage</h4>
            <ol>
              <li>Run <code>{bytePreview(current.lineage.run_id, 24).text}</code></li>
              <li>Effect <code>{bytePreview(String(current.lineage.effect_id), 24).text}</code></li>
              <li>Event <code>{bytePreview(current.lineage.event_id, 24).text}</code></li>
            </ol>
          </div>
          <div className={styles.viewTabs} role="tablist" aria-label="Artifact views">
            <button type="button" role="tab" aria-selected={view === "preview"} onClick={() => setView("preview")}>Preview</button>
            {artifact.name && <button type="button" role="tab" aria-selected={view === "versions"} onClick={() => setView("versions")}>Versions</button>}
            <button type="button" role="tab" aria-selected={view === "release"} onClick={() => setView("release")}>Release</button>
          </div>
          {view === "preview" && (preview.isLoading ? <p className={styles.emptyPreview}>Loading preview…</p> : preview.isError ? <p className={styles.emptyPreview}>Preview could not be loaded.</p> : preview.data ? <PreviewView preview={preview.data.preview} /> : null)}
          {view === "versions" && artifact.name && <VersionsPanel name={artifact.name} />}
          {view === "release" && <ReleasePanel artifact={current} onReleased={() => setView("preview")} />}
        </div>
        <div className={styles.actions}>
          <button type="button" className="secondary-button" onClick={() => download.mutate()} disabled={download.isPending}>{download.isPending ? "Downloading…" : "Download exact bytes"}</button>
          {download.isError && <p className={styles.emptyPreview} role="alert">{download.error instanceof Error ? download.error.message : "Download failed."}</p>}
        </div>
      </div>
    </div>
  );
}
