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

export function secondsToHumanFriendly(seconds) {
  const divisors = [60, 60, 24, 365];
  const units = ['s', 'm', 'h', 'd', 'y'];

  if (Math.abs(seconds) < 60) {
    return seconds + 's';
  }

  // Iterate over the input value (base), continuously moduloing/dividing it in order to break it down into the
  // different unit components.
  let i = 0;
  let base = seconds;
  let parts = [];

  do {
    const remainder = base % divisors[i];
    base = (base - remainder) / divisors[i];

    parts.unshift([remainder, units[i]]);

    i += 1;
  } while (base > 1 && i < divisors.length);

  // Reassemble each unit component to get our final duration string.
  let result = '';
  for (const [value, unit] of parts) {
    result += `${value}${unit} `;
  }

  return result;
}
