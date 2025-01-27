export function bytesToHumanFriendly(bytes) {
  const base = 1024;
  const units = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB', 'ZiB', 'YiB'];
  const decimalPoints = 1;

  if (Math.abs(bytes) < base) {
    return bytes + ' B';
  }

  let u = -1;
  const r = 10 ** decimalPoints;

  do {
    bytes /= base;
    ++u;
  } while (Math.round(Math.abs(bytes) * r) / r >= base && u < units.length - 1);

  return bytes.toFixed(decimalPoints) + ' ' + units[u];
}
