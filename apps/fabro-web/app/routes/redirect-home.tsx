import { Navigate } from "react-router";
import { ErrorState } from "../components/state";
import { ApiError } from "../lib/api-client";
import { useAuthMe } from "../lib/queries";

export default function RedirectHome() {
  const { data, error, mutate } = useAuthMe();

  if (data) {
    return <Navigate to="/runs" replace />;
  }

  if (error instanceof ApiError && error.status === 401) {
    return <Navigate to="/login" replace />;
  }

  if (error) {
    return (
      <main className="min-h-screen bg-page py-12">
        <ErrorState
          title="Backend not connected"
          description="The Fabro backend is not connected, so live runs cannot load yet. Check the preview connection and try again."
          onRetry={() => void mutate()}
        />
      </main>
    );
  }

  return null;
}
