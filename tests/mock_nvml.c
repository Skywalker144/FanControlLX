/* Process tests only: loaded via LD_LIBRARY_PATH in an isolated child.
 * No real driver, sysfs, or physical fan is accessed. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void path(char *out, const char *name) {
    snprintf(out, 4096, "%s/%s", getenv("FANCONTROLLX_MOCK_DIR"), name);
}
static void state(unsigned *policy, unsigned *target) {
    char p[4096]; path(p, "gpu-state");
    FILE *f = fopen(p, "r");
    *policy = 0; *target = 0;
    if (f) { if (fscanf(f, "%u %u", policy, target) != 2) abort(); fclose(f); }
}
static int change(unsigned policy, unsigned target) {
    char p[4096], temp[4096]; path(p, "fail-write");
    if (access(p, F_OK) == 0) return 999;
    path(p, "nvidia-leases.json");
    FILE *journal = fopen(p, "r");
    if (!journal) abort(); /* journaling must precede the first write */
    fclose(journal);
    path(p, "gpu-state"); path(temp, "gpu-state.tmp");
    FILE *f = fopen(temp, "w"); if (!f) abort();
    fprintf(f, "%u %u\n", policy, target); fclose(f); rename(temp, p);
    path(p, "writes"); f = fopen(p, "a");
    fprintf(f, "%u %u\n", policy, target); fclose(f);
    return 0;
}
int nvmlInit_v2(void) { return 0; }
int nvmlShutdown(void) { return 0; }
const char *nvmlErrorString(int code) { (void)code; return "mock driver failure"; }
int nvmlDeviceGetCount_v2(unsigned *v) {
    char p[4096]; path(p, "enumerations"); FILE *f = fopen(p, "a");
    if (f) { fputs("scan\n", f); fclose(f); }
    path(p, "gpu-hidden"); *v = access(p, F_OK) == 0 ? 0 : 1; return 0;
}
int nvmlDeviceGetHandleByIndex_v2(unsigned i, void **h) { if (i) return 999; *h = (void *)1; return 0; }
int nvmlDeviceGetHandleByUUID(const char *uuid, void **h) { if (strcmp(uuid,"GPU-abcd")) return 999; *h=(void *)1; return 0; }
int nvmlDeviceGetUUID(void *h, char *s, unsigned n) { (void)h; snprintf(s,n,"GPU-abcd"); return 0; }
int nvmlDeviceGetName(void *h, char *s, unsigned n) { (void)h; snprintf(s,n,"Mock GPU"); return 0; }
int nvmlDeviceGetNumFans(void *h, unsigned *v) {
    (void)h; char p[4096]; path(p,"fan-count"); FILE *f=fopen(p,"r"); *v=1;
    if(f){if(fscanf(f,"%u",v)!=1)abort();fclose(f);} return 0;
}
int nvmlDeviceGetMinMaxFanSpeed(void *h, unsigned *min, unsigned *max) { (void)h; *min=30; *max=100; return 0; }
int nvmlDeviceGetTemperature(void *h, unsigned sensor, unsigned *v) {
    (void)h; (void)sensor; char p[4096]; path(p,"stall-read");
    if(access(p,F_OK)==0) {
        path(p,"read-started"); FILE *marker=fopen(p,"w");
        if(marker){fputs("started",marker);fclose(marker);} sleep(3);
    }
    path(p,"temperature");
    FILE *f=fopen(p,"r"); *v=40;
    if(f){if(fscanf(f,"%u",v)!=1){fclose(f);return 999;}fclose(f);}
    return 0;
}
int nvmlDeviceGetFanControlPolicy_v2(void *h,unsigned fan,unsigned *v) {
    (void)h;(void)fan;char p[4096];path(p,"fail-read");
    if(access(p,F_OK)==0)return 999;
    unsigned t;state(v,&t);return 0;
}
int nvmlDeviceGetFanSpeed_v2(void *h,unsigned fan,unsigned *v) { (void)h;(void)fan;unsigned p;state(&p,v);return 0; }
int nvmlDeviceGetTargetFanSpeed(void *h,unsigned fan,unsigned *v) { return nvmlDeviceGetFanSpeed_v2(h,fan,v); }
struct fan_speed { unsigned version,fan,speed; };
int nvmlDeviceGetFanSpeedRPM(void *h,struct fan_speed *s) { (void)h;unsigned p,t;state(&p,&t);s->speed=t*20;return 0; }
int nvmlDeviceSetFanSpeed_v2(void *h,unsigned fan,unsigned target) { (void)h;(void)fan;return change(1,target); }
int nvmlDeviceSetDefaultFanSpeed_v2(void *h,unsigned fan) { (void)h;(void)fan;return change(0,0); }
