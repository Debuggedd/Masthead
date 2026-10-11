#!/bin/sh
# Two throwaway kind clusters ("staging", "production") with a mock shop app, metrics-server, Flux and a
# Helm release, for screenshots and demos without real cluster data.
# usage: scripts/demo_cluster.sh [up|down]     needs: docker, kind, kubectl, flux
# Then run the app against it in isolation (own HOME, so your contexts and settings stay out of it):
#   HOME=/tmp/masthead-demo KUBECONFIG=/tmp/masthead-demo/kubeconfig scripts/screenshot.sh out.png
set -e
dir=/tmp/masthead-demo
export KUBECONFIG=$dir/kubeconfig

if [ "$1" = down ]; then
  kind delete cluster --name staging; kind delete cluster --name production; rm -rf "$dir"; exit
fi
mkdir -p "$dir"

for c in staging production; do
  if ! kind get clusters | grep -qx $c; then
    if [ $c = production ]; then
      printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnodes: [{role: control-plane}, {role: worker}, {role: worker}]\n' > "$dir/kind.yaml"
      kind create cluster --name $c --config "$dir/kind.yaml"
    else
      kind create cluster --name $c
    fi
    kubectl config rename-context kind-$c $c
  fi
  kubectl config use-context $c

  kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
  kubectl -n kube-system patch deploy metrics-server --type=json \
    -p='[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--kubelet-insecure-tls"}]' || true
  flux install
  flux create source git podinfo --url=https://github.com/stefanprodan/podinfo --branch=master --interval=10m
  kubectl create namespace podinfo --dry-run=client -o yaml | kubectl apply -f -
  flux create kustomization podinfo --source=GitRepository/podinfo --path=./kustomize --prune=true \
    --target-namespace=podinfo --interval=10m --wait=false
  flux create source helm ingress-nginx --url=https://kubernetes.github.io/ingress-nginx --interval=1h
  flux create helmrelease ingress-nginx --source=HelmRepository/ingress-nginx --chart=ingress-nginx \
    --target-namespace=ingress-nginx --create-target-namespace --interval=10m \
    --values=/dev/stdin <<EOF
controller: {service: {type: ClusterIP}, ingressClassResource: {default: true}}
EOF
  replicas=2; [ $c = production ] && replicas=3
  sed "s/REPLICAS/$replicas/" <<'EOF' | kubectl apply -f -
apiVersion: v1
kind: Namespace
metadata: {name: shop}
---
apiVersion: v1
kind: ConfigMap
metadata: {name: api-config, namespace: shop}
data:
  LOG_LEVEL: info
  FEATURE_FLAGS: "checkout-v2,recommendations"
  config.yaml: |
    server:
      port: 9898
    cache:
      url: redis://redis-master.cache:6379
---
apiVersion: v1
kind: Secret
metadata: {name: api-secrets, namespace: shop}
stringData:
  DATABASE_URL: postgres://shop:not-a-real-password@db.example.internal:5432/shop
  STRIPE_KEY: sk_test_0000000000000000000000000
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: api, namespace: shop, labels: {app: api}}
spec:
  replicas: REPLICAS
  selector: {matchLabels: {app: api}}
  template:
    metadata: {labels: {app: api}}
    spec:
      containers:
      - name: api
        image: ghcr.io/stefanprodan/podinfo:6.7.1
        command: [./podinfo, --port=9898, --level=debug]
        ports: [{containerPort: 9898, name: http}]
        envFrom: [{configMapRef: {name: api-config}}, {secretRef: {name: api-secrets}}]
        resources: {requests: {cpu: 50m, memory: 64Mi}, limits: {cpu: 500m, memory: 256Mi}}
---
apiVersion: v1
kind: Service
metadata: {name: api, namespace: shop}
spec: {selector: {app: api}, ports: [{port: 80, targetPort: 9898, name: http}]}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: frontend, namespace: shop, labels: {app: frontend}}
spec:
  replicas: 2
  selector: {matchLabels: {app: frontend}}
  template:
    metadata: {labels: {app: frontend}}
    spec:
      containers:
      - name: nginx
        image: nginx:1.27-alpine
        ports: [{containerPort: 80, name: http}]
        resources: {requests: {cpu: 20m, memory: 32Mi}, limits: {cpu: 200m, memory: 128Mi}}
---
apiVersion: v1
kind: Service
metadata: {name: frontend, namespace: shop}
spec: {selector: {app: frontend}, ports: [{port: 80, targetPort: 80, name: http}]}
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: shop, namespace: shop}
spec:
  rules:
  - host: shop.example.com
    http:
      paths:
      - {path: /api, pathType: Prefix, backend: {service: {name: api, port: {number: 80}}}}
      - {path: /, pathType: Prefix, backend: {service: {name: frontend, port: {number: 80}}}}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: worker, namespace: shop, labels: {app: worker}}
spec:
  replicas: 2
  selector: {matchLabels: {app: worker}}
  template:
    metadata: {labels: {app: worker}}
    spec:
      containers:
      - name: worker
        image: busybox:1.36
        command: [sh, -c, 'i=0; while true; do i=$((i+1)); o=$((10000+i));
          if [ $((i%29)) = 0 ]; then echo "{\"level\":\"error\",\"msg\":\"order failed: card declined\",\"order\":$o}";
          elif [ $((i%11)) = 0 ]; then echo "{\"level\":\"warn\",\"msg\":\"payment provider slow\",\"order\":$o,\"ms\":$((RANDOM%900+600))}";
          elif [ $((i%5)) = 0 ]; then echo "{\"level\":\"debug\",\"msg\":\"cache hit\",\"key\":\"cart:$o\"}";
          else echo "{\"level\":\"info\",\"msg\":\"processed order\",\"order\":$o,\"ms\":$((RANDOM%90+10))}"; fi; sleep 1; done']
        resources: {requests: {cpu: 10m, memory: 16Mi}, limits: {cpu: 100m, memory: 64Mi}}
---
apiVersion: apps/v1
kind: StatefulSet
metadata: {name: postgres, namespace: shop}
spec:
  serviceName: postgres
  replicas: 1
  selector: {matchLabels: {app: postgres}}
  template:
    metadata: {labels: {app: postgres}}
    spec:
      containers:
      - name: postgres
        image: postgres:17-alpine
        env: [{name: POSTGRES_PASSWORD, value: demo}]
        volumeMounts: [{name: data, mountPath: /var/lib/postgresql/data}]
        resources: {requests: {cpu: 50m, memory: 128Mi}, limits: {memory: 512Mi}}
  volumeClaimTemplates:
  - metadata: {name: data}
    spec: {accessModes: [ReadWriteOnce], resources: {requests: {storage: 2Gi}}}
---
apiVersion: batch/v1
kind: CronJob
metadata: {name: nightly-report, namespace: shop}
spec:
  schedule: "*/10 * * * *"
  jobTemplate:
    spec:
      template:
        spec:
          restartPolicy: Never
          containers: [{name: report, image: busybox:1.36, command: [sh, -c, 'echo generating report; sleep 5; echo done']}]
EOF
done

# Autoscaling, disruption budgets and namespace guardrails, so those pages aren't empty.
for c in staging production; do
  kubectl --context $c apply -f - <<'EOF'
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata: {name: api, namespace: shop}
spec:
  scaleTargetRef: {apiVersion: apps/v1, kind: Deployment, name: api}
  minReplicas: 2
  maxReplicas: 6
  metrics: [{type: Resource, resource: {name: cpu, target: {type: Utilization, averageUtilization: 80}}}]
---
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata: {name: api, namespace: shop}
spec: {minAvailable: 1, selector: {matchLabels: {app: api}}}
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: api-ingress, namespace: shop}
spec:
  podSelector: {matchLabels: {app: api}}
  policyTypes: [Ingress]
  ingress: [{from: [{podSelector: {matchLabels: {app: frontend}}}]}]
---
apiVersion: v1
kind: ResourceQuota
metadata: {name: shop, namespace: shop}
spec: {hard: {pods: "40", requests.cpu: "4", requests.memory: 8Gi}}
---
apiVersion: v1
kind: LimitRange
metadata: {name: defaults, namespace: shop}
spec:
  limits: [{type: Container, default: {cpu: 200m, memory: 128Mi}, defaultRequest: {cpu: 20m, memory: 32Mi}}]
EOF
done

# Something to show on the Problems page.
kubectl --context production apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: payments, namespace: shop, labels: {app: payments}}
spec:
  replicas: 1
  selector: {matchLabels: {app: payments}}
  template:
    metadata: {labels: {app: payments}}
    spec:
      containers:
      - name: payments
        image: busybox:1.36
        command: [sh, -c, 'echo "connecting to payments gateway"; sleep 3; echo "fatal: PAYMENTS_TOKEN not set" >&2; exit 1']
EOF
kubectl --context production -n shop create job report-manual --from=cronjob/nightly-report 2>/dev/null || true
# Both contexts selected in the isolated app config.
mkdir -p "$dir/Library/Application Support/Masthead"
printf '[["staging","%s"],["production","%s"]]' "$KUBECONFIG" "$KUBECONFIG" > "$dir/Library/Application Support/Masthead/contexts.json"
echo "ready: HOME=$dir KUBECONFIG=$KUBECONFIG"
