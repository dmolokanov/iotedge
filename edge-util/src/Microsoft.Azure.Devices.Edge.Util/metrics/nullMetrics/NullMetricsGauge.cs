// Copyright (c) Microsoft. All rights reserved.
namespace Microsoft.Azure.Devices.Edge.Util.Metrics.NullMetrics
{
    public class NullMetricsGauge : IMetricsGauge
    {
        public void Set(long value, string[] labelValues)
        {
        }
    }
}
