<script>
var lastDate = 0;
var data = [];
const TICKINTERVAL = 86400000;
const XAXISRANGE = 777600000;

function getDayWiseTimeSeries(baseval, count, yrange) {
    var i = 0;
    while (i < count) {
        var x = baseval;
        var y = Math.floor(Math.random() * (yrange.max - yrange.min + 1)) + yrange.min;

        data.push({ x, y });
        lastDate = baseval;
        baseval += TICKINTERVAL;
        i++;
    }
}

getDayWiseTimeSeries(new Date('11 Feb 2017 GMT').getTime(), 10, {
    min: 10,
    max: 90
});

function getNewSeries(baseval, yrange) {
    var newDate = baseval + TICKINTERVAL;
    lastDate = newDate;

    for (var i = 0; i < data.length - 10; i++) {
        // IMPORTANT
        // we reset the x and y of the data which is out of drawing area
        // to prevent memory leaks
        data[i].x = newDate - XAXISRANGE - TICKINTERVAL;
        data[i].y = 0;
    }

    data.push({
        x: newDate,
        y: Math.floor(Math.random() * (yrange.max - yrange.min + 1)) + yrange.min
    });
}

function resetData() {
    // Alternatively, you can also reset the data at certain intervals to prevent creating a huge series
    data = data.slice(data.length - 10, data.length);
}

export default {
    name: 'Vue Chart',
    data: function () {
        return {
            series: [
                {
                    data: data.slice()
                }
            ],
            options: {
                chart: {
                    id: 'realtime',
                    type: 'line',
                    animations: {
                        enabled: true,
                        easing: 'linear',
                        dynamicAnimation: {
                            speed: 2000
                        }
                    },
                    toolbar: {
                        show: false
                    },
                    zoom: {
                        enabled: false
                    }
                },
                dataLabels: {
                    enabled: false
                },
                stroke: {
                    curve: 'smooth'
                },
                markers: {
                    size: 0
                },
                tooltip: {
                    enabled: false
                },
                xaxis: {
                    type: 'datetime',
                    range: XAXISRANGE
                },
                legend: {
                    show: false
                }
            }
        };
    },
    async mounted() {
        var me = this;
        window.setInterval(() => {
            getNewSeries(lastDate, {
                min: 10,
                max: 90
            });

            me.$refs.chart.updateSeries([{ data: data }]);
        }, 2000);

        // every 60 seconds, we reset the data to prevent memory leaks
        window.setInterval(() => {
            resetData();

            me.$refs.chart.updateSeries([{ data }], false, true);
        }, 60000);
    }
};
</script>

<template>
    <div class="col-span-12">
        <div class="card w-full mb-4">
            <span class="block text-color font-medium mb-4 text-3xl">Ingest Throughput</span>
            <div class="h-64">
                <apexchart ref="chart" height="100%" type="line" :options="options" :series="series"></apexchart>
            </div>
        </div>
    </div>
</template>
