import { Link } from "react-router-dom";
import { Empty, useDocumentTitle } from "../components/Bits";

export default function NotFound() {
  useDocumentTitle("Not found");
  return (
    <div className="page">
      <Empty title="Nothing here">That address does not match a page. <Link to="/">Back to repositories</Link>.</Empty>
    </div>
  );
}
