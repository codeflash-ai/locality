type SnapshotLike = {
  connection: {
    status: string;
  };
};

export function connectionReady(snapshot: SnapshotLike) {
  return snapshot.connection.status === "active";
}

export function connectionMissing(snapshot: SnapshotLike) {
  return !connectionReady(snapshot);
}
