import { lazy, Suspense } from "react";
import { Route, Routes } from "react-router-dom";
import { Shell } from "./components/Shell";
import { RepoLayout } from "./components/RepoLayout";
import { Loading } from "./components/Bits";

const RepoList = lazy(() => import("./pages/RepoList"));
const RepoHome = lazy(() => import("./pages/RepoHome"));
const CodePage = lazy(() => import("./pages/CodePage"));
const CommitsPage = lazy(() => import("./pages/CommitsPage"));
const CommitPage = lazy(() => import("./pages/CommitPage"));
const BranchesPage = lazy(() => import("./pages/BranchesPage"));
const PullsPage = lazy(() => import("./pages/PullsPage"));
const PullPage = lazy(() => import("./pages/PullPage"));
const ChecksPage = lazy(() => import("./pages/ChecksPage"));
const NotFound = lazy(() => import("./pages/NotFound"));

export function App() {
  return (
    <Shell>
      <Suspense fallback={<Loading />}>
        <Routes>
          <Route path="/" element={<RepoList />} />
          <Route path="/:repo" element={<RepoLayout />}>
            <Route index element={<RepoHome />} />
            <Route path="tree/*" element={<CodePage kind="tree" />} />
            <Route path="blob/*" element={<CodePage kind="blob" />} />
            <Route path="commits/*" element={<CommitsPage />} />
            <Route path="commit/:id" element={<CommitPage />} />
            <Route path="branches" element={<BranchesPage />} />
            <Route path="pulls" element={<PullsPage />} />
            <Route path="pull/:n" element={<PullPage />} />
            <Route path="checks" element={<ChecksPage />} />
            <Route path="*" element={<NotFound />} />
          </Route>
          <Route path="*" element={<NotFound />} />
        </Routes>
      </Suspense>
    </Shell>
  );
}
