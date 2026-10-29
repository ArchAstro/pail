import { Link } from "react-router-dom";
import type { TreeListing } from "../../shared/types";
import { bytes } from "../lib/format";
import { blobPath, treePath } from "../lib/routes";
import { Icon } from "./Icon";

export function TreeTable({ repo, listing, refName }: { repo: string; listing: TreeListing; refName: string }) {
  const up = listing.path ? listing.path.split("/").slice(0, -1).join("/") : null;
  return (
    <table className="tree">
      <caption className="sr-only">Files in {listing.path || "the repository root"}</caption>
      <thead className="sr-only"><tr><th>Name</th><th>Size</th></tr></thead>
      <tbody>
        {up !== null && (
          <tr><td colSpan={2}><Link to={treePath(repo, refName, up)} className="tree__name"><Icon name="folder" size={16} className="ico-dir" />..</Link></td></tr>
        )}
        {listing.entries.map((e) => {
          const p = listing.path ? `${listing.path}/${e.name}` : e.name;
          return (
            <tr key={e.name}>
              <td>
                <Link to={e.type === "tree" ? treePath(repo, refName, p) : blobPath(repo, refName, p)} className="tree__name">
                  <Icon name={e.type === "tree" ? "folder" : "file"} size={16} className={e.type === "tree" ? "ico-dir" : "ico-file"} />
                  {e.name}
                </Link>
              </td>
              <td className="tree__size">{e.size !== undefined ? bytes(e.size) : ""}</td>
            </tr>
          );
        })}
        {listing.entries.length === 0 && <tr><td colSpan={2} className="muted pad">This directory is empty.</td></tr>}
      </tbody>
    </table>
  );
}
